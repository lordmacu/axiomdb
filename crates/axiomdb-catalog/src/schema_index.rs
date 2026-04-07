// ── IndexDef ──────────────────────────────────────────────────────────────────

/// Metadata for an index — one row in `axiom_indexes`.
///
/// ## On-disk format
///
/// ```text
/// [index_id:4 LE][table_id:4 LE][root_page_id:8 LE][flags:1][name_len:1][name bytes]
/// [ncols:1][col_idx:2 LE, order:1]×ncols
/// [pred_len:2 LE][pred_sql: pred_len UTF-8 bytes]   ← Phase 6.7; omitted if predicate = None
/// ```
///
/// The `columns` section (starting at `ncols`) is optional — old rows that
/// predate Phase 6.1 end after `name bytes`.  `from_bytes` returns
/// `columns: vec![]` for such rows (backward-compatible).
///
/// The `pred_len` section is optional — old rows without it get `predicate = None`
/// (backward-compatible with pre-6.7 databases).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// Auto-incremented unique ID, allocated by `CatalogWriter::create_index`.
    /// `0` is reserved (invalid / not yet assigned).
    pub index_id: u32,
    pub table_id: TableId,
    pub name: String,
    pub root_page_id: u64,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Ordered list of columns that form the index key.
    ///
    /// Empty for indexes created before Phase 6.1 (treated as unusable by planner).
    pub columns: Vec<IndexColumnDef>,
    /// WHERE predicate as a SQL expression string (Phase 6.7).
    ///
    /// `None` = full index (covers all rows).
    /// `Some(sql)` = partial index; only rows satisfying the predicate are indexed.
    ///
    /// Example: `Some("deleted_at IS NULL")` or `Some("status = 'active'")`.
    pub predicate: Option<String>,
    /// Target leaf-page fill factor in percent (Phase 6.8).
    ///
    /// Controls when a leaf page splits during INSERT. A leaf page with
    /// `num_keys >= ceil(ORDER_LEAF × fillfactor / 100)` triggers a split,
    /// leaving the specified percentage of capacity used and `100 - fillfactor`
    /// free for future inserts without another split.
    ///
    /// Valid range: 10–100. Default: 90 (matches PostgreSQL `BTREE_DEFAULT_FILLFACTOR`).
    /// `fillfactor = 100` → pages fill completely before splitting (current behavior).
    ///
    /// Stored as 1 byte after the predicate section. Absent in pre-6.8 rows;
    /// `from_bytes` returns 90 for backward compatibility.
    pub fillfactor: u8,
    /// `true` for FK auto-indexes that use composite keys `(fk_val | RecordId)`.
    ///
    /// Allows multiple rows with the same FK value (InnoDB approach: append 10
    /// RecordId bytes to the key to guarantee global uniqueness in the B-Tree).
    ///
    /// Stored in `flags bit 2` (`0x04`). Pre-6.9 rows have bit 2 = 0 → `false`.
    pub is_fk_index: bool,
    /// INCLUDE columns for covering index scans (Phase 6.13).
    ///
    /// Catalog-only metadata in Phase 6.13. B-Tree leaf storage of these values
    /// is planned for Phase 6.15.
    ///
    /// Stored after `is_fk_index`: `[include_len: 1 byte][col_idx: u16 LE] × len`.
    /// Absent in pre-6.13 rows → `include_columns = vec![]`.
    pub include_columns: Vec<u16>,
    /// Index type (Phase 11.1b): 0 = BTree (default), 1 = Brin.
    ///
    /// Stored as 1 byte after include_columns. Pre-11.1b rows default to 0 (BTree).
    pub index_type: u8,
    /// BRIN: heap pages per summary range (Phase 11.1b).
    ///
    /// Default 128. Only meaningful when `index_type == 1`.
    /// Stored as 4 bytes LE after index_type, only when `index_type != 0`.
    pub pages_per_range: u32,
}

impl IndexDef {
    /// Serializes to binary row format.
    ///
    /// Format:
    /// `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
    /// `[ncols:1][col_idx:2 LE, order:1]×ncols`
    /// `[pred_len:2 LE][pred_sql bytes]` — only present if predicate is Some
    ///
    /// - `flags bit0` = unique, `flags bit1` = primary key, `flags bit2` = fk_index
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "index name too long");
        debug_assert!(self.columns.len() <= 63, "too many index columns");

        let mut flags: u8 = 0;
        if self.is_unique {
            flags |= 0x01;
        }
        if self.is_primary {
            flags |= 0x02;
        }
        if self.is_fk_index {
            flags |= 0x04;
        }

        let pred_bytes = self.predicate.as_deref().map(str::as_bytes);
        let pred_len = pred_bytes.map(|b| b.len()).unwrap_or(0);

        let mut buf = Vec::with_capacity(
            4 + 4
                + 8
                + 1
                + 1
                + name.len()
                + 1
                + self.columns.len() * 3
                + if pred_len > 0 { 2 + pred_len } else { 0 },
        );
        buf.extend_from_slice(&self.index_id.to_le_bytes());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.root_page_id.to_le_bytes());
        buf.push(flags);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        // Columns section
        buf.push(self.columns.len() as u8);
        for col in &self.columns {
            buf.extend_from_slice(&col.col_idx.to_le_bytes());
            buf.push(col.order as u8);
        }
        // Predicate section (Phase 6.7) — always write pred_len u16 (0 if no predicate).
        // Pre-6.7 rows have no pred_len bytes; from_bytes skips when < 2 bytes remain.
        buf.extend_from_slice(&(pred_len as u16).to_le_bytes());
        if let Some(pred) = pred_bytes {
            buf.extend_from_slice(pred);
        }
        // Fill factor (Phase 6.8) — always written as 1 byte after predicate.
        // Pre-6.8 readers stop before this byte and use the default of 90.
        buf.push(self.fillfactor);
        // Include columns (Phase 6.13) — after fillfactor, always written.
        buf.push(self.include_columns.len() as u8);
        for &col_idx in &self.include_columns {
            buf.extend_from_slice(&col_idx.to_le_bytes());
        }
        // Index type (Phase 11.1b) — 1 byte: 0=BTree, 1=Brin.
        buf.push(self.index_type);
        // BRIN pages_per_range — 4 bytes LE, only when index_type != 0.
        if self.index_type != 0 {
            buf.extend_from_slice(&self.pages_per_range.to_le_bytes());
        }
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Backward-compatible: if the `ncols` byte is absent (old row format), returns `columns: vec![]`.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated IndexRow bytes".into(),
            position: None,
        };

        // Fixed header: 4 (index_id) + 4 (table_id) + 8 (root_page_id) + 1 (flags) + 1 (name_len) = 18
        if bytes.len() < 18 {
            return Err(err());
        }

        let index_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let table_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let root_page_id = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let flags = bytes[16];
        let is_unique = flags & 0x01 != 0;
        let is_primary = flags & 0x02 != 0;
        let is_fk_index = flags & 0x04 != 0; // Phase 6.9
        let name_len = bytes[17] as usize;

        if bytes.len() < 18 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[18..18 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in index name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = 18 + name_len;

        // Backward-compatible: columns section is optional
        let columns = if bytes.len() > consumed {
            let ncols = bytes[consumed] as usize;
            consumed += 1;
            let mut cols = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                if bytes.len() < consumed + 3 {
                    return Err(err());
                }
                let col_idx = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]);
                let order = SortOrder::try_from(bytes[consumed + 2])?;
                consumed += 3;
                cols.push(IndexColumnDef { col_idx, order });
            }
            cols
        } else {
            vec![]
        };

        // Predicate section (Phase 6.7) — backward-compatible:
        // only present when 2+ bytes remain after the columns section.
        let predicate = if bytes.len() >= consumed + 2 {
            let pred_len = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if pred_len == 0 {
                None
            } else {
                if bytes.len() < consumed + pred_len {
                    return Err(DbError::ParseError {
                        message: "IndexDef predicate section truncated".into(),
                        position: None,
                    });
                }
                let sql = std::str::from_utf8(&bytes[consumed..consumed + pred_len])
                    .map_err(|_| DbError::ParseError {
                        message: "IndexDef predicate not valid UTF-8".into(),
                        position: None,
                    })?
                    .to_string();
                consumed += pred_len;
                Some(sql)
            }
        } else {
            None // pre-6.7 row — no predicate bytes
        };

        // Fill factor (Phase 6.8) — 1 byte after predicate. Pre-6.8 rows lack
        // this byte; default to 90 (PostgreSQL BTREE_DEFAULT_FILLFACTOR).
        let fillfactor = if bytes.len() > consumed {
            let ff = bytes[consumed];
            consumed += 1;
            ff
        } else {
            90 // default for pre-6.8 rows
        };

        // Include columns (Phase 6.13) — [include_len: 1][col_idx: u16 LE] × len.
        // Absent in pre-6.13 rows → empty vec.
        let include_columns = if bytes.len() > consumed {
            let include_len = bytes[consumed] as usize;
            consumed += 1;
            let mut cols = Vec::with_capacity(include_len);
            for _ in 0..include_len {
                if bytes.len() < consumed + 2 {
                    return Err(DbError::ParseError {
                        message: "IndexDef include_columns truncated".into(),
                        position: None,
                    });
                }
                let col_idx = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]);
                consumed += 2;
                cols.push(col_idx);
            }
            cols
        } else {
            vec![] // pre-6.13 row
        };

        // Index type (Phase 11.1b) — 1 byte. Pre-11.1b rows default to 0 (BTree).
        let index_type = if bytes.len() > consumed {
            let it = bytes[consumed];
            consumed += 1;
            it
        } else {
            0 // BTree default
        };

        // BRIN pages_per_range — 4 bytes LE, only when index_type != 0.
        let pages_per_range = if index_type != 0 && bytes.len() >= consumed + 4 {
            let ppr = u32::from_le_bytes([
                bytes[consumed],
                bytes[consumed + 1],
                bytes[consumed + 2],
                bytes[consumed + 3],
            ]);
            consumed += 4;
            ppr
        } else {
            128 // default
        };

        Ok((
            Self {
                index_id,
                table_id,
                name,
                root_page_id,
                is_unique,
                is_primary,
                columns,
                predicate,
                fillfactor,
                is_fk_index,
                include_columns,
                index_type,
                pages_per_range,
            },
            consumed,
        ))
    }
}
