//! Catalog schema types and their binary serialization.
//!
//! These types represent rows in the system tables:
//! - `axiom_tables`      → [`TableDef`]
//! - `axiom_columns`     → [`ColumnDef`]
//! - `axiom_indexes`     → [`IndexDef`]
//! - `axiom_constraints` → [`ConstraintDef`] (Phase 4.22b)
//! - `axiom_databases`   → [`DatabaseDef`] (Phase 22b.3a)
//! - `axiom_table_databases` → [`TableDatabaseDef`] (Phase 22b.3a)
//!
//! ## Binary row format
//!
//! Each type has a compact, length-prefixed binary format for storage in heap
//! slots. All multi-byte integers are little-endian. String names are stored as
//! a 1-byte length prefix followed by the UTF-8 bytes (max 255 bytes per name).
//!
//! **TableRow**: `[table_id:4][schema_len:1][schema bytes][name_len:1][name bytes]`
//!
//! **ColumnRow**: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
//! - `flags bit0` = nullable
//!
//! **IndexRow**: `[index_id:4][table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
//! - `flags bit0` = unique, `flags bit1` = primary key

use axiomdb_core::error::DbError;

include!("schema_database.rs");
include!("schema_table.rs");
include!("schema_aggregate.rs");
include!("schema_index.rs");
include!("schema_constraints.rs");

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ColumnType ────────────────────────────────────────────────────────────

    #[test]
    fn test_column_type_roundtrip_all_variants() {
        let variants = [
            ColumnType::Bool,
            ColumnType::Int,
            ColumnType::BigInt,
            ColumnType::Float,
            ColumnType::Text,
            ColumnType::Bytes,
            ColumnType::Timestamp,
            ColumnType::Uuid,
            ColumnType::Json,
            ColumnType::Jsonb,
            ColumnType::Decimal,
            ColumnType::Date,
        ];
        for v in variants {
            let byte: u8 = v.into();
            let back = ColumnType::try_from(byte).expect("roundtrip failed");
            assert_eq!(back, v, "roundtrip failed for {v:?}");
        }
    }

    #[test]
    fn test_column_type_invalid_discriminant() {
        assert!(ColumnType::try_from(0).is_err());
        assert!(ColumnType::try_from(13).is_err());
        assert!(ColumnType::try_from(255).is_err());
    }

    // ── DatabaseDef ───────────────────────────────────────────────────────────

    #[test]
    fn test_database_def_roundtrip() {
        let def = DatabaseDef {
            name: "ventas".into(),
            default_collation: None,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = DatabaseDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_database_def_roundtrip() {
        let def = TableDatabaseDef {
            table_id: 42,
            database_name: "analytics".into(),
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDatabaseDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    // ── TableDef ──────────────────────────────────────────────────────────────

    #[test]
    fn test_table_def_roundtrip() {
        let def = TableDef {
            id: 42,
            root_page_id: 7,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".to_string(),
            table_name: "users".to_string(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_roundtrip_with_root_page() {
        // Verify that root_page_id round-trips correctly for various values.
        for &root in &[1u64, 100, u64::MAX / 2, u64::MAX - 1] {
            let def = TableDef {
                id: 1,
                root_page_id: root,
                storage_layout: TableStorageLayout::Heap,
                schema_name: "public".into(),
                table_name: "t".into(),
                schema_version: 1,
                immutable: false,
                persistence: TablePersistence::Permanent,
                relation_kind: RelationKind::Table,
                defining_query: None,
                default_collation: None,
                triggers: vec![],
            };
            let (back, _) = TableDef::from_bytes(&def.to_bytes()).unwrap();
            assert_eq!(back.root_page_id, root);
        }
    }

    #[test]
    fn test_table_def_empty_strings() {
        let def = TableDef {
            id: 1,
            root_page_id: 5,
            storage_layout: TableStorageLayout::Heap,
            schema_name: String::new(),
            table_name: String::new(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        let (back, _) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn test_table_def_truncated_input_error() {
        let def = TableDef {
            id: 1,
            root_page_id: 3,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "s".into(),
            table_name: "t".into(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        // Minimum is 14 bytes; truncate to 10 (has id+root but no schema_len).
        assert!(TableDef::from_bytes(&bytes[..10]).is_err());
        // Old 3-byte truncation still fails.
        assert!(TableDef::from_bytes(&bytes[..3]).is_err());
    }

    #[test]
    fn test_table_def_roundtrip_clustered_layout() {
        let def = TableDef {
            id: 9,
            root_page_id: 77,
            storage_layout: TableStorageLayout::Clustered,
            schema_name: "public".into(),
            table_name: "orders".into(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_legacy_bytes_decode_as_heap() {
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&42u32.to_le_bytes());
        legacy.extend_from_slice(&99u64.to_le_bytes());
        legacy.push(6);
        legacy.extend_from_slice(b"public");
        legacy.push(5);
        legacy.extend_from_slice(b"users");

        let (back, consumed) = TableDef::from_bytes(&legacy).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.root_page_id, 99);
        assert_eq!(back.storage_layout, TableStorageLayout::Heap);
        assert_eq!(back.schema_name, "public");
        assert_eq!(back.table_name, "users");
        assert_eq!(back.persistence, TablePersistence::Permanent);
        assert_eq!(back.relation_kind, RelationKind::Table);
        assert_eq!(back.defining_query, None);
        assert_eq!(consumed, legacy.len());
    }

    #[test]
    fn test_table_def_roundtrip_unlogged_persistence() {
        let def = TableDef {
            id: 17,
            root_page_id: 55,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".into(),
            table_name: "events".into(),
            schema_version: 4,
            immutable: false,
            persistence: TablePersistence::Unlogged,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_table_def_roundtrip_materialized_view() {
        let def = TableDef {
            id: 33,
            root_page_id: 91,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".into(),
            table_name: "mv_sales".into(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::MaterializedView,
            defining_query: Some("SELECT region, SUM(total) FROM sales GROUP BY region".into()),
            default_collation: Some("es".into()),
            triggers: vec![],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = TableDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
        assert!(back.is_materialized_view());
    }

    // ── ColumnDef ─────────────────────────────────────────────────────────────

    #[test]
    fn test_column_def_roundtrip_nullable() {
        let def = ColumnDef {
            table_id: 5,
            col_idx: 2,
            name: "email".to_string(),
            col_type: ColumnType::Text,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: Some("es".into()),
            generated_stored: false,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = ColumnDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_column_def_roundtrip_not_nullable() {
        let def = ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: "id".to_string(),
            col_type: ColumnType::BigInt,
            nullable: false,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
        };
        let bytes = def.to_bytes();
        let (back, _) = ColumnDef::from_bytes(&bytes).unwrap();
        assert!(!back.nullable);
        assert_eq!(back.col_type, ColumnType::BigInt);
    }

    #[test]
    fn test_column_def_truncated_input_error() {
        let def = ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: "x".into(),
            col_type: ColumnType::Int,
            nullable: false,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
        };
        let bytes = def.to_bytes();
        assert!(ColumnDef::from_bytes(&bytes[..5]).is_err());
    }

    #[test]
    fn test_column_def_roundtrip_generated_stored() {
        let def = ColumnDef {
            table_id: 1,
            col_idx: 2,
            name: "total".into(),
            col_type: ColumnType::Int,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: Some("price * qty".into()),
            collation: None,
            generated_stored: true,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = ColumnDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    // ── IndexDef ──────────────────────────────────────────────────────────────

    #[test]
    fn test_index_def_roundtrip_primary_unique() {
        let def = IndexDef {
            index_id: 1,
            table_id: 3,
            name: "users_pkey".to_string(),
            root_page_id: 77,
            is_unique: true,
            is_primary: true,
            columns: vec![IndexColumnDef {
                col_idx: 0,
                order: SortOrder::Asc,
                expr: None,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_index_def_roundtrip_non_unique() {
        let def = IndexDef {
            index_id: 5,
            table_id: 2,
            name: "orders_user_id_idx".to_string(),
            root_page_id: 100,
            is_unique: false,
            is_primary: false,
            columns: vec![IndexColumnDef {
                col_idx: 2,
                order: SortOrder::Asc,
                expr: None,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, _) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back.index_id, 5);
        assert!(!back.is_unique);
        assert!(!back.is_primary);
        assert_eq!(back.columns.len(), 1);
        assert_eq!(back.columns[0].col_idx, 2);
    }

    #[test]
    fn test_index_def_truncated_input_error() {
        let def = IndexDef {
            index_id: 1,
            table_id: 1,
            name: "x".into(),
            root_page_id: 0,
            is_unique: false,
            is_primary: false,
            columns: vec![],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        assert!(IndexDef::from_bytes(&bytes[..10]).is_err());
    }

    #[test]
    fn test_index_def_roundtrip_multi_column() {
        let def = IndexDef {
            index_id: 7,
            table_id: 4,
            name: "composite_idx".to_string(),
            root_page_id: 200,
            is_unique: false,
            is_primary: false,
            columns: vec![
                IndexColumnDef {
                    col_idx: 1,
                    order: SortOrder::Asc,
                    expr: None,
                },
                IndexColumnDef {
                    col_idx: 3,
                    order: SortOrder::Desc,
                    expr: None,
                },
            ],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let bytes = def.to_bytes();
        let (back, consumed) = IndexDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_index_def_old_format_backward_compat() {
        // Simulate an old-format row that ends after the name (no columns section).
        let def = IndexDef {
            index_id: 2,
            table_id: 1,
            name: "old_idx".to_string(),
            root_page_id: 50,
            is_unique: false,
            is_primary: false,
            columns: vec![],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        };
        let full_bytes = def.to_bytes();
        // Truncate the columns section (last byte is ncols=0, remove it).
        let old_bytes = &full_bytes[..full_bytes.len() - 1];
        let (back, consumed) = IndexDef::from_bytes(old_bytes).unwrap();
        assert_eq!(back.columns, vec![]);
        assert_eq!(consumed, old_bytes.len());
    }
}
