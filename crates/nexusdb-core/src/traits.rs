use crate::Result;

/// Identificador de página en el storage.
pub type PageId = u64;

/// Identificador de fila (página + slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    pub page_id: PageId,
    pub slot_id: u16,
}

/// Identificador de transacción.
pub type TxnId = u64;

/// Trait central del motor de almacenamiento.
/// Implementaciones: MmapStorage, MemoryStorage, EncryptedStorage.
pub trait StorageEngine: Send + Sync {
    fn read_page(&self, page_id: PageId) -> Result<Box<[u8; 8192]>>;
    fn write_page(&self, page_id: PageId, data: &[u8; 8192]) -> Result<()>;
    fn alloc_page(&self) -> Result<PageId>;
    fn free_page(&self, page_id: PageId) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn total_pages(&self) -> u64;
}

/// Trait central de índice.
/// Implementaciones: BTreeIndex, HashIndex, HnswIndex, FtsIndex.
pub trait Index: Send + Sync {
    fn insert(&self, key: &[u8], rid: RecordId) -> Result<()>;
    fn delete(&self, key: &[u8], rid: RecordId) -> Result<()>;
    fn lookup(&self, key: &[u8]) -> Result<Vec<RecordId>>;
    fn range(
        &self,
        lo: std::ops::Bound<&[u8]>,
        hi: std::ops::Bound<&[u8]>,
    ) -> Result<Vec<RecordId>>;
}
