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

/// Compact field-level delta for clustered in-place updates.
/// Stores only the changed field bytes instead of full row images.
#[derive(Debug, Clone)]
pub struct ClusteredFieldPatchEntry {
    pub key: Vec<u8>,
    pub old_header: RowHeader,
    pub new_header: RowHeader,
    pub old_row_data: Vec<u8>, // full old row_data (for undo)
    pub field_deltas: Vec<FieldDelta>,
}

/// One changed field: offset within row_data + old/new bytes.
///
/// `old_bytes` and `new_bytes` are stored inline as `[u8; 8]` to avoid heap
/// allocation. Fixed-size field types (Bool=1B, Int/Date=4B, BigInt/Real/
/// Timestamp=8B) always fit. Read `size` bytes: `&old_bytes[..size as usize]`.
#[derive(Debug, Clone)]
pub struct FieldDelta {
    pub offset: u16,
    pub size: u8,
    pub old_bytes: [u8; 8],
    pub new_bytes: [u8; 8],
}

impl ClusteredFieldPatchEntry {
    fn encode_header(hdr: &RowHeader) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&hdr.txn_id_created.to_le_bytes());
        buf[8..16].copy_from_slice(&hdr.txn_id_deleted.to_le_bytes());
        buf[16..20].copy_from_slice(&hdr.row_version.to_le_bytes());
        buf[20..24].copy_from_slice(&hdr._flags.to_le_bytes());
        buf
    }

    /// Encode the old_value for WAL: [RowHeader:24][num_fields:1][offset:2][size:1][bytes:N]...
    ///
    /// The on-disk byte layout is identical to when `old_bytes` was `Vec<u8>`;
    /// only `size` bytes are written from the inline `[u8; 8]` array.
    pub fn encode_old_value(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24 + 1 + self.field_deltas.len() * 12);
        buf.extend_from_slice(&Self::encode_header(&self.old_header));
        buf.push(self.field_deltas.len() as u8);
        for delta in &self.field_deltas {
            buf.extend_from_slice(&delta.offset.to_le_bytes());
            buf.push(delta.size);
            buf.extend_from_slice(&delta.old_bytes[..delta.size as usize]);
        }
        buf
    }

    /// Encode the new_value for WAL: [RowHeader:24][num_fields:1][offset:2][size:1][bytes:N]...
    ///
    /// The on-disk byte layout is identical to when `new_bytes` was `Vec<u8>`;
    /// only `size` bytes are written from the inline `[u8; 8]` array.
    pub fn encode_new_value(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24 + 1 + self.field_deltas.len() * 12);
        buf.extend_from_slice(&Self::encode_header(&self.new_header));
        buf.push(self.field_deltas.len() as u8);
        for delta in &self.field_deltas {
            buf.extend_from_slice(&delta.offset.to_le_bytes());
            buf.push(delta.size);
            buf.extend_from_slice(&delta.new_bytes[..delta.size as usize]);
        }
        buf
    }

    /// Decode a clustered field-patch entry from WAL `old_value`/`new_value`
    /// payloads.
    ///
    /// `old_row_data` is intentionally left empty: the WAL only stores the
    /// changed field bytes plus the old/new row headers.
    pub fn from_wal_values(
        key: &[u8],
        old_value: &[u8],
        new_value: &[u8],
    ) -> Result<Self, DbError> {
        let (old_header, old_fields) = decode_patch_value(old_value, "old_value")?;
        let (new_header, new_fields) = decode_patch_value(new_value, "new_value")?;

        if old_fields.len() != new_fields.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered field patch delta count mismatch: old has {}, new has {}",
                    old_fields.len(),
                    new_fields.len()
                ),
            });
        }

        let mut field_deltas = Vec::with_capacity(old_fields.len());
        for ((old_offset, old_size, old_arr), (new_offset, new_size, new_arr)) in
            old_fields.into_iter().zip(new_fields.into_iter())
        {
            if old_offset != new_offset || old_size != new_size {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "clustered field patch delta shape mismatch: old=({old_offset},{old_size}) new=({new_offset},{new_size})"
                    ),
                });
            }

            field_deltas.push(FieldDelta {
                offset: old_offset,
                size: old_size,
                old_bytes: old_arr,
                new_bytes: new_arr,
            });
        }

        Ok(Self {
            key: key.to_vec(),
            old_header,
            new_header,
            old_row_data: Vec::new(),
            field_deltas,
        })
    }
}

type DecodedPatchFields = Vec<(u16, u8, [u8; 8])>;

fn decode_patch_value(
    bytes: &[u8],
    field_name: &str,
) -> Result<(RowHeader, DecodedPatchFields), DbError> {
    const PATCH_HEADER_LEN: usize = 24;
    const COUNT_LEN: usize = 1;
    const DELTA_META_LEN: usize = 3;

    if bytes.len() < PATCH_HEADER_LEN + COUNT_LEN {
        return Err(DbError::InvalidValue {
            reason: format!(
                "clustered field patch {field_name} truncated: need at least {} bytes, got {}",
                PATCH_HEADER_LEN + COUNT_LEN,
                bytes.len()
            ),
        });
    }

    let row_header = RowHeader {
        txn_id_created: u64::from_le_bytes(bytes[0..8].try_into().unwrap_or([0u8; 8])),
        txn_id_deleted: u64::from_le_bytes(bytes[8..16].try_into().unwrap_or([0u8; 8])),
        row_version: u32::from_le_bytes(bytes[16..20].try_into().unwrap_or([0u8; 4])),
        _flags: u32::from_le_bytes(bytes[20..24].try_into().unwrap_or([0u8; 4])),
    };

    let num_fields = bytes[PATCH_HEADER_LEN] as usize;
    let mut cursor = PATCH_HEADER_LEN + COUNT_LEN;
    let mut fields = Vec::with_capacity(num_fields);

    for idx in 0..num_fields {
        if cursor + DELTA_META_LEN > bytes.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered field patch {field_name} delta {idx} truncated before metadata"
                ),
            });
        }

        let offset = u16::from_le_bytes([bytes[cursor], bytes[cursor + 1]]);
        let size = bytes[cursor + 2];
        cursor += DELTA_META_LEN;

        if size as usize > 8 {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered field patch {field_name} delta {idx} size {size} exceeds 8 (max fixed field size)"
                ),
            });
        }

        let end = cursor + size as usize;
        if end > bytes.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered field patch {field_name} delta {idx} truncated: size {size} exceeds payload"
                ),
            });
        }

        let mut arr = [0u8; 8];
        arr[..size as usize].copy_from_slice(&bytes[cursor..end]);
        fields.push((offset, size, arr));
        cursor = end;
    }

    if cursor != bytes.len() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "clustered field patch {field_name} has {} trailing bytes after decoding",
                bytes.len() - cursor
            ),
        });
    }

    Ok((row_header, fields))
}

#[cfg(test)]
mod tests {
    use super::{ClusteredFieldPatchEntry, ClusteredRowImage, FieldDelta};
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

    #[test]
    fn clustered_field_patch_roundtrip_from_wal_values() {
        let mut old0 = [0u8; 8];
        old0[..4].copy_from_slice(&[1, 2, 3, 4]);
        let mut new0 = [0u8; 8];
        new0[..4].copy_from_slice(&[5, 6, 7, 8]);
        let mut old1 = [0u8; 8];
        old1[0] = 0;
        let mut new1 = [0u8; 8];
        new1[0] = 1;

        let patch = ClusteredFieldPatchEntry {
            key: b"pk".to_vec(),
            old_header: row_header(7, 0, 3),
            new_header: row_header(8, 0, 4),
            old_row_data: b"old row body".to_vec(),
            field_deltas: vec![
                FieldDelta {
                    offset: 1,
                    size: 4,
                    old_bytes: old0,
                    new_bytes: new0,
                },
                FieldDelta {
                    offset: 9,
                    size: 1,
                    old_bytes: old1,
                    new_bytes: new1,
                },
            ],
        };

        let decoded = ClusteredFieldPatchEntry::from_wal_values(
            &patch.key,
            &patch.encode_old_value(),
            &patch.encode_new_value(),
        )
        .unwrap();

        assert_eq!(decoded.key, patch.key);
        assert_eq!(
            decoded.old_header.txn_id_created,
            patch.old_header.txn_id_created
        );
        assert_eq!(
            decoded.old_header.txn_id_deleted,
            patch.old_header.txn_id_deleted
        );
        assert_eq!(decoded.old_header.row_version, patch.old_header.row_version);
        assert_eq!(decoded.old_header._flags, patch.old_header._flags);
        assert_eq!(
            decoded.new_header.txn_id_created,
            patch.new_header.txn_id_created
        );
        assert_eq!(
            decoded.new_header.txn_id_deleted,
            patch.new_header.txn_id_deleted
        );
        assert_eq!(decoded.new_header.row_version, patch.new_header.row_version);
        assert_eq!(decoded.new_header._flags, patch.new_header._flags);
        assert!(decoded.old_row_data.is_empty());
        assert_eq!(decoded.field_deltas.len(), 2);
        assert_eq!(decoded.field_deltas[0].offset, 1);
        assert_eq!(decoded.field_deltas[0].size, 4);
        assert_eq!(decoded.field_deltas[0].old_bytes[..4], [1, 2, 3, 4]);
        assert_eq!(decoded.field_deltas[0].new_bytes[..4], [5, 6, 7, 8]);
        assert_eq!(decoded.field_deltas[1].offset, 9);
        assert_eq!(decoded.field_deltas[1].size, 1);
        assert_eq!(decoded.field_deltas[1].old_bytes[0], 0);
        assert_eq!(decoded.field_deltas[1].new_bytes[0], 1);
    }
}
