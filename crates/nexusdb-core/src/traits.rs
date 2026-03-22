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

/// Trait de índice — implementaciones: BTreeIndex, HashIndex, HnswIndex, FtsIndex.
/// Nota: StorageEngine vive en nexusdb-storage (usa Page/PageType, evita ciclo de deps).
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
