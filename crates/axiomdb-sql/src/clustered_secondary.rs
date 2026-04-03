//! Clustered-first secondary-index helpers for Phase 39.9.
//!
//! This module models the physical secondary entry as:
//!
//! `secondary_logical_key ++ missing_primary_key_columns`
//!
//! The existing `axiomdb-index::BTree` is reused only as an ordered container.
//! The legacy fixed-size `RecordId` payload remains a compatibility artifact and
//! is never treated as the logical bookmark for clustered rows.

use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_catalog::IndexDef;
use axiomdb_core::{error::DbError, RecordId};
use axiomdb_index::BTree;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;

use crate::key_encoding::{decode_index_key, encode_index_key, MAX_INDEX_KEY};

const DUMMY_RID: RecordId = RecordId {
    page_id: 0,
    slot_id: 0,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusteredSecondaryLayout {
    index_name: String,
    fillfactor: u8,
    is_unique: bool,
    pub secondary_cols: Vec<u16>,
    pub primary_cols: Vec<u16>,
    pub suffix_cols: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClusteredSecondaryEntry {
    pub physical_key: Vec<u8>,
    pub logical_key: Vec<Value>,
    pub primary_key: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusteredSecondaryUpdateOutcome {
    Unchanged,
    Inserted,
    Deleted,
    Replaced,
}

impl ClusteredSecondaryLayout {
    pub fn derive(secondary_idx: &IndexDef, primary_idx: &IndexDef) -> Result<Self, DbError> {
        if secondary_idx.is_primary {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered secondary layout requires a non-primary index, got '{}'",
                    secondary_idx.name
                ),
            });
        }
        if secondary_idx.columns.is_empty() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered secondary layout requires indexed columns on '{}'",
                    secondary_idx.name
                ),
            });
        }
        if !primary_idx.is_primary || primary_idx.columns.is_empty() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered secondary layout requires a populated primary index for table {}",
                    secondary_idx.table_id
                ),
            });
        }
        if secondary_idx.table_id != primary_idx.table_id {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "secondary index '{}' and primary index '{}' belong to different tables",
                    secondary_idx.name, primary_idx.name
                ),
            });
        }

        let secondary_cols: Vec<u16> = secondary_idx.columns.iter().map(|c| c.col_idx).collect();
        let primary_cols: Vec<u16> = primary_idx.columns.iter().map(|c| c.col_idx).collect();
        let suffix_cols: Vec<u16> = primary_cols
            .iter()
            .copied()
            .filter(|pk_col| !secondary_cols.contains(pk_col))
            .collect();

        Ok(Self {
            index_name: secondary_idx.name.clone(),
            fillfactor: secondary_idx.fillfactor,
            is_unique: secondary_idx.is_unique,
            secondary_cols,
            primary_cols,
            suffix_cols,
        })
    }

    pub fn physical_value_count(&self) -> usize {
        self.secondary_cols.len() + self.suffix_cols.len()
    }

    pub fn logical_prefix_bounds(
        &self,
        logical_prefix: &[Value],
    ) -> Result<(Vec<u8>, Vec<u8>), DbError> {
        if logical_prefix.len() > self.secondary_cols.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered secondary '{}' received {} prefix values for {} key columns",
                    self.index_name,
                    logical_prefix.len(),
                    self.secondary_cols.len()
                ),
            });
        }

        let lo = encode_index_key(logical_prefix)?;
        let mut hi = lo.clone();
        hi.resize(MAX_INDEX_KEY, 0xFF);
        Ok((lo, hi))
    }

    pub fn entry_from_row(
        &self,
        row: &[Value],
    ) -> Result<Option<ClusteredSecondaryEntry>, DbError> {
        let logical_key = self.collect_row_values(row, &self.secondary_cols, false)?;
        if logical_key.iter().any(|v| matches!(v, Value::Null)) {
            return Ok(None);
        }

        let suffix_vals = self.collect_row_values(row, &self.suffix_cols, true)?;
        let primary_key = self.collect_row_values(row, &self.primary_cols, true)?;

        let mut physical_vals = logical_key.clone();
        physical_vals.extend(suffix_vals);
        let physical_key = encode_index_key(&physical_vals)?;

        Ok(Some(ClusteredSecondaryEntry {
            physical_key,
            logical_key,
            primary_key,
        }))
    }

    pub fn decode_entry_key(
        &self,
        physical_key: &[u8],
    ) -> Result<ClusteredSecondaryEntry, DbError> {
        let (decoded, consumed) = decode_index_key(physical_key, self.physical_value_count())?;
        if consumed != physical_key.len() {
            return Err(DbError::ParseError {
                message: format!(
                    "clustered secondary '{}' key has trailing bytes after decoding {} values",
                    self.index_name,
                    self.physical_value_count()
                ),
                position: None,
            });
        }

        let logical_len = self.secondary_cols.len();
        let logical_key = decoded[..logical_len].to_vec();
        let suffix_vals = &decoded[logical_len..];

        let mut primary_key = Vec::with_capacity(self.primary_cols.len());
        for pk_col in &self.primary_cols {
            if let Some(pos) = self.secondary_cols.iter().position(|col| col == pk_col) {
                primary_key.push(logical_key[pos].clone());
                continue;
            }
            if let Some(pos) = self.suffix_cols.iter().position(|col| col == pk_col) {
                primary_key.push(suffix_vals[pos].clone());
                continue;
            }
            return Err(DbError::Internal {
                message: format!(
                    "clustered secondary '{}' could not reconstruct primary key column {}",
                    self.index_name, pk_col
                ),
            });
        }

        Ok(ClusteredSecondaryEntry {
            physical_key: physical_key.to_vec(),
            logical_key,
            primary_key,
        })
    }

    pub fn scan_prefix(
        &self,
        storage: &dyn StorageEngine,
        root_page_id: u64,
        logical_prefix: &[Value],
    ) -> Result<Vec<ClusteredSecondaryEntry>, DbError> {
        let (lo, hi) = self.logical_prefix_bounds(logical_prefix)?;
        let pairs = BTree::range_in(storage, root_page_id, Some(&lo), Some(&hi))?;
        pairs
            .into_iter()
            .map(|(_rid, key)| self.decode_entry_key(&key))
            .collect()
    }

    pub fn insert_row(
        &self,
        storage: &mut dyn StorageEngine,
        root_page_id: &AtomicU64,
        row: &[Value],
    ) -> Result<bool, DbError> {
        let Some(entry) = self.entry_from_row(row)? else {
            return Ok(false);
        };

        if self.is_unique {
            self.ensure_unique_logical_key_absent(
                storage,
                root_page_id.load(Ordering::Acquire),
                &entry.logical_key,
                None,
            )?;
        }

        BTree::insert_in(
            storage,
            root_page_id,
            &entry.physical_key,
            DUMMY_RID,
            self.fillfactor,
        )?;
        Ok(true)
    }

    pub fn delete_row(
        &self,
        storage: &mut dyn StorageEngine,
        root_page_id: &AtomicU64,
        row: &[Value],
    ) -> Result<bool, DbError> {
        let Some(entry) = self.entry_from_row(row)? else {
            return Ok(false);
        };
        BTree::delete_in(storage, root_page_id, &entry.physical_key)
    }

    pub fn update_row(
        &self,
        storage: &mut dyn StorageEngine,
        root_page_id: &AtomicU64,
        old_row: &[Value],
        new_row: &[Value],
    ) -> Result<ClusteredSecondaryUpdateOutcome, DbError> {
        let old_entry = self.entry_from_row(old_row)?;
        let new_entry = self.entry_from_row(new_row)?;

        match (old_entry, new_entry) {
            (None, None) => Ok(ClusteredSecondaryUpdateOutcome::Unchanged),
            (Some(old), Some(new)) if old.physical_key == new.physical_key => {
                Ok(ClusteredSecondaryUpdateOutcome::Unchanged)
            }
            (None, Some(new)) => {
                if self.is_unique {
                    self.ensure_unique_logical_key_absent(
                        storage,
                        root_page_id.load(Ordering::Acquire),
                        &new.logical_key,
                        None,
                    )?;
                }
                BTree::insert_in(
                    storage,
                    root_page_id,
                    &new.physical_key,
                    DUMMY_RID,
                    self.fillfactor,
                )?;
                Ok(ClusteredSecondaryUpdateOutcome::Inserted)
            }
            (Some(old), None) => {
                let _ = BTree::delete_in(storage, root_page_id, &old.physical_key)?;
                Ok(ClusteredSecondaryUpdateOutcome::Deleted)
            }
            (Some(old), Some(new)) => {
                if self.is_unique {
                    self.ensure_unique_logical_key_absent(
                        storage,
                        root_page_id.load(Ordering::Acquire),
                        &new.logical_key,
                        Some(&old.physical_key),
                    )?;
                }
                let _ = BTree::delete_in(storage, root_page_id, &old.physical_key)?;
                BTree::insert_in(
                    storage,
                    root_page_id,
                    &new.physical_key,
                    DUMMY_RID,
                    self.fillfactor,
                )?;
                Ok(ClusteredSecondaryUpdateOutcome::Replaced)
            }
        }
    }

    fn ensure_unique_logical_key_absent(
        &self,
        storage: &dyn StorageEngine,
        root_page_id: u64,
        logical_key: &[Value],
        excluded_physical_key: Option<&[u8]>,
    ) -> Result<(), DbError> {
        let matches = self.scan_prefix(storage, root_page_id, logical_key)?;
        let conflict = matches.into_iter().any(|entry| {
            excluded_physical_key
                .map(|excluded| entry.physical_key.as_slice() != excluded)
                .unwrap_or(true)
        });

        if conflict {
            return Err(DbError::UniqueViolation {
                index_name: self.index_name.clone(),
                value: logical_key.first().map(|v| format!("{v}")),
            });
        }

        Ok(())
    }

    fn collect_row_values(
        &self,
        row: &[Value],
        cols: &[u16],
        reject_null: bool,
    ) -> Result<Vec<Value>, DbError> {
        let mut values = Vec::with_capacity(cols.len());
        for col_idx in cols {
            let value =
                row.get(*col_idx as usize)
                    .cloned()
                    .ok_or_else(|| DbError::InvalidValue {
                        reason: format!(
                            "clustered secondary '{}' requires column {} in row with len {}",
                            self.index_name,
                            col_idx,
                            row.len()
                        ),
                    })?;

            if reject_null && matches!(value, Value::Null) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "clustered secondary '{}' cannot build a primary-key bookmark with NULL column {}",
                        self.index_name, col_idx
                    ),
                });
            }

            values.push(value);
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::{IndexColumnDef, SortOrder};
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use axiomdb_storage::{MemoryStorage, Page, PageType, StorageEngine};

    fn idx_col(col_idx: u16) -> IndexColumnDef {
        IndexColumnDef {
            col_idx,
            order: SortOrder::Asc,
        }
    }

    fn primary_idx(cols: &[u16]) -> IndexDef {
        IndexDef {
            index_id: 1,
            table_id: 7,
            name: "users_pkey".into(),
            root_page_id: 10,
            is_unique: true,
            is_primary: true,
            columns: cols.iter().copied().map(idx_col).collect(),
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        }
    }

    fn secondary_idx(name: &str, unique: bool, cols: &[u16]) -> IndexDef {
        IndexDef {
            index_id: 2,
            table_id: 7,
            name: name.into(),
            root_page_id: 11,
            is_unique: unique,
            is_primary: false,
            columns: cols.iter().copied().map(idx_col).collect(),
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        }
    }

    fn alloc_index_root(storage: &mut MemoryStorage) -> u64 {
        let root = storage.alloc_page(PageType::Index).unwrap();
        let mut page = Page::new(PageType::Index, root);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(root, &page).unwrap();
        root
    }

    #[test]
    fn derive_layout_skips_overlapping_primary_columns() {
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_email_tenant", false, &[2, 0]),
            &primary_idx(&[0, 1]),
        )
        .unwrap();

        assert_eq!(layout.secondary_cols, vec![2, 0]);
        assert_eq!(layout.primary_cols, vec![0, 1]);
        assert_eq!(layout.suffix_cols, vec![1]);
        assert_eq!(layout.physical_value_count(), 3);
    }

    #[test]
    fn entry_from_row_roundtrips_through_decode() {
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status", false, &[2]),
            &primary_idx(&[0, 1]),
        )
        .unwrap();
        let row = vec![
            Value::Int(7),
            Value::Int(42),
            Value::Text("active".into()),
            Value::Text("ignored".into()),
        ];

        let entry = layout.entry_from_row(&row).unwrap().unwrap();
        let decoded = layout.decode_entry_key(&entry.physical_key).unwrap();

        assert_eq!(decoded.logical_key, vec![Value::Text("active".into())]);
        assert_eq!(decoded.primary_key, vec![Value::Int(7), Value::Int(42)]);
    }

    #[test]
    fn update_row_is_noop_when_secondary_and_primary_keys_stay_stable() {
        let mut storage = MemoryStorage::new();
        let root = AtomicU64::new(alloc_index_root(&mut storage));
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status", false, &[2]),
            &primary_idx(&[0]),
        )
        .unwrap();

        let old_row = vec![
            Value::Int(9),
            Value::Text("payload-a".into()),
            Value::Text("active".into()),
        ];
        let new_row = vec![
            Value::Int(9),
            Value::Text("payload-b".into()),
            Value::Text("active".into()),
        ];

        assert_eq!(
            layout
                .update_row(&mut storage, &root, &old_row, &new_row)
                .unwrap(),
            ClusteredSecondaryUpdateOutcome::Unchanged
        );
        assert!(
            BTree::range_in(&storage, root.load(Ordering::Acquire), None, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn insert_row_unique_rejects_duplicate_logical_key() {
        let mut storage = MemoryStorage::new();
        let root = AtomicU64::new(alloc_index_root(&mut storage));
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("uq_email", true, &[1]),
            &primary_idx(&[0]),
        )
        .unwrap();

        let row_a = vec![Value::Int(1), Value::Text("a@example.com".into())];
        let row_b = vec![Value::Int(2), Value::Text("a@example.com".into())];

        assert!(layout.insert_row(&mut storage, &root, &row_a).unwrap());
        let err = layout.insert_row(&mut storage, &root, &row_b).unwrap_err();
        assert!(matches!(
            err,
            DbError::UniqueViolation { ref index_name, .. } if index_name == "uq_email"
        ));
    }

    #[test]
    fn entry_from_row_with_null_secondary_returns_none() {
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status", false, &[2]),
            &primary_idx(&[0]),
        )
        .unwrap();
        // Column 2 (secondary key) is NULL → no index entry.
        let row = vec![Value::Int(1), Value::Text("payload".into()), Value::Null];
        assert!(layout.entry_from_row(&row).unwrap().is_none());
    }

    #[test]
    fn delete_row_removes_entry_from_index() {
        let mut storage = MemoryStorage::new();
        let root = AtomicU64::new(alloc_index_root(&mut storage));
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status", false, &[2]),
            &primary_idx(&[0]),
        )
        .unwrap();

        let row = vec![
            Value::Int(5),
            Value::Text("payload".into()),
            Value::Text("active".into()),
        ];
        assert!(layout.insert_row(&mut storage, &root, &row).unwrap());

        // Verify entry exists.
        let before = layout
            .scan_prefix(
                &storage,
                root.load(Ordering::Acquire),
                &[Value::Text("active".into())],
            )
            .unwrap();
        assert_eq!(before.len(), 1);

        // Delete it.
        assert!(layout.delete_row(&mut storage, &root, &row).unwrap());

        // Verify entry gone.
        let after = layout
            .scan_prefix(
                &storage,
                root.load(Ordering::Acquire),
                &[Value::Text("active".into())],
            )
            .unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn update_row_replace_deletes_old_and_inserts_new() {
        let mut storage = MemoryStorage::new();
        let root = AtomicU64::new(alloc_index_root(&mut storage));
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status", false, &[2]),
            &primary_idx(&[0]),
        )
        .unwrap();

        let old_row = vec![
            Value::Int(1),
            Value::Text("payload".into()),
            Value::Text("active".into()),
        ];
        layout.insert_row(&mut storage, &root, &old_row).unwrap();

        let new_row = vec![
            Value::Int(1),
            Value::Text("payload".into()),
            Value::Text("inactive".into()),
        ];
        let outcome = layout
            .update_row(&mut storage, &root, &old_row, &new_row)
            .unwrap();
        assert_eq!(outcome, ClusteredSecondaryUpdateOutcome::Replaced);

        // Old key gone.
        let old_entries = layout
            .scan_prefix(
                &storage,
                root.load(Ordering::Acquire),
                &[Value::Text("active".into())],
            )
            .unwrap();
        assert!(old_entries.is_empty());

        // New key present.
        let new_entries = layout
            .scan_prefix(
                &storage,
                root.load(Ordering::Acquire),
                &[Value::Text("inactive".into())],
            )
            .unwrap();
        assert_eq!(new_entries.len(), 1);
        assert_eq!(new_entries[0].primary_key, vec![Value::Int(1)]);
    }

    #[test]
    fn composite_secondary_key_encodes_and_decodes_correctly() {
        let layout = ClusteredSecondaryLayout::derive(
            &secondary_idx("idx_status_name", false, &[2, 1]),
            &primary_idx(&[0]),
        )
        .unwrap();
        assert_eq!(layout.secondary_cols, vec![2, 1]);
        assert_eq!(layout.suffix_cols, vec![0]); // PK col 0 not in secondary
        assert_eq!(layout.physical_value_count(), 3); // 2 sec + 1 suffix

        let row = vec![
            Value::Int(42),
            Value::Text("Alice".into()),
            Value::Text("active".into()),
        ];
        let entry = layout.entry_from_row(&row).unwrap().unwrap();
        let decoded = layout.decode_entry_key(&entry.physical_key).unwrap();

        assert_eq!(
            decoded.logical_key,
            vec![Value::Text("active".into()), Value::Text("Alice".into())]
        );
        assert_eq!(decoded.primary_key, vec![Value::Int(42)]);
    }
}
