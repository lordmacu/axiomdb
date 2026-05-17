//! Tests for the versioned `ResolvedTable` cache in `SessionContext`.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md`.
//!
//! Step 1 (this file at first): unit tests of the new
//! `SessionContext::get_table_if_version` accessor on a hand-built
//! `ResolvedTable`. Subsequent steps add integration tests that drive
//! the full SQL stack.

use axiomdb_catalog::resolver::ResolvedTable;
use axiomdb_catalog::schema::{
    RelationKind, TableDef, TablePersistence, TableStorageLayout, DEFAULT_DATABASE_NAME,
};
use axiomdb_sql::SessionContext;

fn fake_resolved_table(id: u32, name: &str, schema_version: u64) -> ResolvedTable {
    ResolvedTable {
        def: TableDef {
            id,
            root_page_id: 1,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".into(),
            table_name: name.into(),
            schema_version,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        },
        columns: vec![],
        indexes: vec![],
        constraints: vec![],
        foreign_keys: vec![],
    }
}

#[test]
fn get_table_if_version_returns_some_on_match() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(
        DEFAULT_DATABASE_NAME,
        "public",
        "t",
        fake_resolved_table(1, "t", 7),
    );
    let r = ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 7);
    assert!(r.is_some(), "cache hit expected when version matches");
    assert_eq!(r.unwrap().def.id, 1);
}

#[test]
fn get_table_if_version_returns_none_on_version_mismatch() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(
        DEFAULT_DATABASE_NAME,
        "public",
        "t",
        fake_resolved_table(1, "t", 7),
    );
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 8)
            .is_none(),
        "version 8 must miss when cached is 7"
    );
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 6)
            .is_none(),
        "version 6 must miss when cached is 7 (older version means stale)"
    );
}

#[test]
fn get_table_if_version_returns_none_on_miss() {
    let ctx = SessionContext::default();
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 0)
            .is_none(),
        "miss expected when nothing was cached"
    );
}
