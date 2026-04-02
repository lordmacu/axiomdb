use axiomdb_core::error::DbError;
use axiomdb_storage::RowHeader;

const ROOT_PID_LEN: usize = 8;
const TXN_ID_CREATED_LEN: usize = 8;
const TXN_ID_DELETED_LEN: usize = 8;
const ROW_VERSION_LEN: usize = 4;
const FLAGS_LEN: usize = 4;
const ROW_LEN_LEN: usize = 4;
const FIXED_LEN: usize = ROOT_PID_LEN
    + TXN_ID_CREATED_LEN
    + TXN_ID_DELETED_LEN
    + ROW_VERSION_LEN
    + FLAGS_LEN
    + ROW_LEN_LEN;

/// Exact logical clustered row image stored in WAL for rollback and future
/// crash recovery support.
///
/// `row_data` always contains the full logical row bytes, even when the row is
/// overflow-backed on disk.
#[derive(Debug, Clone)]
pub struct ClusteredRowImage {
    pub root_pid: u64,
    pub row_header: RowHeader,
    pub row_data: Vec<u8>,
}

impl ClusteredRowImage {
    pub fn new(root_pid: u64, row_header: RowHeader, row_data: &[u8]) -> Self {
        Self {
            root_pid,
            row_header,
            row_data: row_data.to_vec(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, DbError> {
        if self.row_data.len() > u32::MAX as usize {
            return Err(DbError::ValueTooLarge {
                len: self.row_data.len(),
                max: u32::MAX as usize,
            });
        }

        let mut out = Vec::with_capacity(FIXED_LEN + self.row_data.len());
        out.extend_from_slice(&self.root_pid.to_le_bytes());
        out.extend_from_slice(&self.row_header.txn_id_created.to_le_bytes());
        out.extend_from_slice(&self.row_header.txn_id_deleted.to_le_bytes());
        out.extend_from_slice(&self.row_header.row_version.to_le_bytes());
        out.extend_from_slice(&self.row_header._flags.to_le_bytes());
        out.extend_from_slice(&(self.row_data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.row_data);
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DbError> {
        if bytes.len() < FIXED_LEN {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered WAL row image truncated: need at least {FIXED_LEN} bytes, got {}",
                    bytes.len()
                ),
            });
        }

        let root_pid = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let txn_id_created = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let txn_id_deleted = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);
        let row_version = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let flags = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
        let row_len = u32::from_le_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]) as usize;

        if bytes.len() != FIXED_LEN + row_len {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered WAL row image length mismatch: header says {row_len} bytes, payload has {}",
                    bytes.len().saturating_sub(FIXED_LEN)
                ),
            });
        }

        Ok(Self {
            root_pid,
            row_header: RowHeader {
                txn_id_created,
                txn_id_deleted,
                row_version,
                _flags: flags,
            },
            row_data: bytes[FIXED_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ClusteredRowImage;
    use axiomdb_storage::RowHeader;

    fn row_header(txn_id: u64, deleted: u64, version: u32) -> RowHeader {
        RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: deleted,
            row_version: version,
            _flags: 7,
        }
    }

    #[test]
    fn clustered_row_image_roundtrip_inline() {
        let image = ClusteredRowImage::new(42, row_header(3, 0, 1), b"hello");
        let decoded = ClusteredRowImage::from_bytes(&image.to_bytes().unwrap()).unwrap();

        assert_eq!(decoded.root_pid, 42);
        assert_eq!(decoded.row_header.txn_id_created, 3);
        assert_eq!(decoded.row_header.txn_id_deleted, 0);
        assert_eq!(decoded.row_header.row_version, 1);
        assert_eq!(decoded.row_header._flags, 7);
        assert_eq!(decoded.row_data, b"hello");
    }

    #[test]
    fn clustered_row_image_roundtrip_large_payload() {
        let payload = vec![b'x'; 12_000];
        let image = ClusteredRowImage::new(99, row_header(11, 0, 2), &payload);
        let decoded = ClusteredRowImage::from_bytes(&image.to_bytes().unwrap()).unwrap();

        assert_eq!(decoded.root_pid, 99);
        assert_eq!(decoded.row_header.txn_id_created, 11);
        assert_eq!(decoded.row_header.row_version, 2);
        assert_eq!(decoded.row_data, payload);
    }
}
