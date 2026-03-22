use nexusdb_core::error::DbError;

pub const PAGE_SIZE: usize = 16_384;
pub const HEADER_SIZE: usize = 64;
pub const PAGE_MAGIC: u64 = 0x4E455855_53444200; // "NEXUSDB\0"

// ── PageType ──────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    Meta = 0,
    Data = 1,
    Index = 2,
    Overflow = 3,
    Free = 4,
}

impl TryFrom<u8> for PageType {
    type Error = DbError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(PageType::Meta),
            1 => Ok(PageType::Data),
            2 => Ok(PageType::Index),
            3 => Ok(PageType::Overflow),
            4 => Ok(PageType::Free),
            _ => Err(DbError::ParseError {
                message: format!("page_type desconocido: {v}"),
            }),
        }
    }
}

// ── PageHeader ────────────────────────────────────────────────────────────────

/// Cabecera de 64 bytes (1 cache line). `repr(C)` garantiza layout exacto.
///
/// El checksum cubre únicamente el body `[HEADER_SIZE..PAGE_SIZE]`, no el
/// header mismo, para evitar incluir el campo `checksum` en su propio cálculo.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct PageHeader {
    /// Número mágico para detectar archivo inválido o corrupción catastrófica.
    pub magic: u64,
    /// Tipo de página.
    pub page_type: u8,
    /// Flags de estado (dirty, pinned, compressed…).
    pub flags: u8,
    /// Número de ítems almacenados en el body.
    pub item_count: u16,
    /// CRC32c del body `[HEADER_SIZE..PAGE_SIZE]`.
    pub checksum: u32,
    /// Identificador lógico de la página (su índice en el archivo).
    pub page_id: u64,
    /// Log Sequence Number del último write que tocó esta página.
    pub lsn: u64,
    /// Offset desde el inicio de la página donde empieza el espacio libre.
    pub free_start: u16,
    /// Offset desde el inicio de la página donde termina el espacio libre.
    pub free_end: u16,
    pub _reserved: [u8; 28],
}

// Verificación en tiempo de compilación: el header debe ocupar exactamente 64 bytes.
const _: () = assert!(
    std::mem::size_of::<PageHeader>() == HEADER_SIZE,
    "PageHeader debe ser exactamente 64 bytes"
);

// ── Page ──────────────────────────────────────────────────────────────────────

/// Página de 16KB alineada a 64 bytes (cache line).
///
/// Internamente es un buffer crudo `[u8; PAGE_SIZE]`; el header se accede
/// mediante punteros raw para lograr acceso zero-copy sin serialización.
#[repr(C, align(64))]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

// Verificaciones en tiempo de compilación.
const _: () = assert!(
    std::mem::size_of::<Page>() == PAGE_SIZE,
    "Page debe ser exactamente PAGE_SIZE bytes"
);
const _: () = assert!(
    std::mem::align_of::<Page>() == 64,
    "Page debe estar alineada a 64 bytes"
);

impl Page {
    /// Crea una página nueva inicializada con header válido y body a cero.
    pub fn new(page_type: PageType, page_id: u64) -> Self {
        let mut page = Page {
            data: [0u8; PAGE_SIZE],
        };

        let hdr = page.header_mut();
        hdr.magic = PAGE_MAGIC;
        hdr.page_type = page_type as u8;
        hdr.flags = 0;
        hdr.item_count = 0;
        hdr.page_id = page_id;
        hdr.lsn = 0;
        hdr.free_start = HEADER_SIZE as u16;
        hdr.free_end = PAGE_SIZE as u16;
        hdr._reserved = [0u8; 28];

        page.update_checksum();
        page
    }

    /// Construye una `Page` desde bytes crudos verificando magic y checksum.
    /// Usado al leer desde disco o mmap.
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self, DbError> {
        let page = Page { data: bytes };

        let magic = page.header().magic;
        if magic != PAGE_MAGIC {
            return Err(DbError::ChecksumMismatch {
                page_id: page.header().page_id,
                expected: PAGE_MAGIC as u32,
                got: magic as u32,
            });
        }

        page.verify_checksum()?;
        Ok(page)
    }

    /// Referencia al header interpretando los primeros 64 bytes.
    ///
    /// # SAFETY
    /// `data` tiene `PAGE_SIZE >= HEADER_SIZE` bytes y `Page` es `repr(C,
    /// align(64))`, por lo que los primeros 64 bytes están correctamente
    /// alineados para `PageHeader` (`repr(C)`). `PageHeader` no tiene padding
    /// oculto (verificado por el assert de tamaño).
    pub fn header(&self) -> &PageHeader {
        unsafe { &*(self.data.as_ptr() as *const PageHeader) }
    }

    /// Referencia mutable al header.
    ///
    /// # SAFETY
    /// Mismas garantías que `header()`. La mutabilidad es segura porque `Page`
    /// posee el buffer completo y ningún otro alias existe.
    pub fn header_mut(&mut self) -> &mut PageHeader {
        unsafe { &mut *(self.data.as_mut_ptr() as *mut PageHeader) }
    }

    /// Slice del body (bytes tras el header).
    pub fn body(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    /// Slice mutable del body.
    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.data[HEADER_SIZE..]
    }

    /// Bytes crudos de la página completa (para escribir a disco).
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    /// Verifica que el CRC32c del body coincide con `header.checksum`.
    pub fn verify_checksum(&self) -> Result<(), DbError> {
        let expected = self.header().checksum;
        let got = crc32c::crc32c(self.body());
        if expected != got {
            return Err(DbError::ChecksumMismatch {
                page_id: self.header().page_id,
                expected,
                got,
            });
        }
        Ok(())
    }

    /// Recalcula y escribe el CRC32c del body en el header.
    /// Llamar siempre antes de hacer flush a disco.
    pub fn update_checksum(&mut self) {
        let checksum = crc32c::crc32c(self.body());
        self.header_mut().checksum = checksum;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_sizes() {
        assert_eq!(std::mem::size_of::<PageHeader>(), HEADER_SIZE);
        assert_eq!(std::mem::size_of::<Page>(), PAGE_SIZE);
        assert_eq!(std::mem::align_of::<Page>(), 64);
    }

    #[test]
    fn test_new_page_is_valid() {
        let page = Page::new(PageType::Data, 42);
        assert_eq!(page.header().magic, PAGE_MAGIC);
        assert_eq!(page.header().page_id, 42);
        assert_eq!(page.header().page_type, PageType::Data as u8);
        assert!(page.verify_checksum().is_ok());
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let mut page = Page::new(PageType::Data, 1);
        // Corromper un byte en el body
        page.data[HEADER_SIZE + 10] ^= 0xFF;
        assert!(page.verify_checksum().is_err());
    }

    #[test]
    fn test_invalid_magic_is_rejected() {
        let mut bytes = [0u8; PAGE_SIZE];
        // Magic incorrecto
        bytes[0..8].copy_from_slice(&0xDEADBEEFu64.to_ne_bytes());
        assert!(Page::from_bytes(bytes).is_err());
    }

    #[test]
    fn test_from_bytes_roundtrip() {
        let page = Page::new(PageType::Index, 99);
        let bytes = *page.as_bytes();
        let page2 = Page::from_bytes(bytes).expect("roundtrip debe ser válido");
        assert_eq!(page2.header().page_id, 99);
        assert_eq!(page2.header().page_type, PageType::Index as u8);
    }

    #[test]
    fn test_update_checksum_after_body_write() {
        let mut page = Page::new(PageType::Data, 5);
        // Escribir en el body sin actualizar checksum → inválido
        page.data[HEADER_SIZE] = 0xAB;
        assert!(page.verify_checksum().is_err());
        // Actualizar checksum → válido
        page.update_checksum();
        assert!(page.verify_checksum().is_ok());
    }

    #[test]
    fn test_page_types_roundtrip() {
        for (byte, expected) in [
            (0u8, PageType::Meta),
            (1, PageType::Data),
            (2, PageType::Index),
            (3, PageType::Overflow),
            (4, PageType::Free),
        ] {
            assert_eq!(PageType::try_from(byte).unwrap(), expected);
        }
        assert!(PageType::try_from(99).is_err());
    }
}
