//! Tests for cursor-reuse-cross-statement (Attack 5).
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-cursor-reuse-cross-statement.md`
//!
//! Step 5.2 (this file at first): unit tests for the SessionContext
//! `clustered_leaf_hint` slot API. Subsequent steps add end-to-end
//! integration tests that drive the full SQL stack through
//! `lookup_with_hint`.

use axiomdb_sql::SessionContext;
use axiomdb_storage::clustered_tree::LeafCursorHint;

fn fake_hint(table_id: u32, root: u64, leaf: u64, version: u64) -> LeafCursorHint {
    LeafCursorHint {
        table_id,
        root_page_id: root,
        leaf_page_id: leaf,
        min_key: vec![0],
        max_key: vec![255],
        schema_version: version,
    }
}

#[test]
fn leaf_hint_starts_absent() {
    let ctx = SessionContext::default();
    assert!(!ctx.clustered_leaf_hint_present());
    assert!(ctx
        .get_clustered_leaf_hint(1, 100, 1, &[10u8])
        .is_none());
}

#[test]
fn leaf_hint_set_then_get_within_range() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    assert!(ctx.clustered_leaf_hint_present());

    let h = ctx
        .get_clustered_leaf_hint(1, 100, 1, &[10u8])
        .expect("hint must match — key 10 is in [0, 255]");
    assert_eq!(h.leaf_page_id, 200);
    assert_eq!(h.table_id, 1);
}

#[test]
fn leaf_hint_get_returns_none_on_mismatches() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));

    // Different table_id.
    assert!(ctx
        .get_clustered_leaf_hint(2, 100, 1, &[10u8])
        .is_none());
    // Different root.
    assert!(ctx
        .get_clustered_leaf_hint(1, 999, 1, &[10u8])
        .is_none());
    // Different schema_version (DDL bump).
    assert!(ctx
        .get_clustered_leaf_hint(1, 100, 2, &[10u8])
        .is_none());
    // Key outside [min_key=[0], max_key=[255]] — use 2-byte key to force
    // a comparison that returns out-of-range.
    assert!(ctx
        .get_clustered_leaf_hint(1, 100, 1, &[255u8, 1u8])
        .is_none());
}

#[test]
fn leaf_hint_invalidate_clears() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    ctx.invalidate_clustered_leaf_hint();
    assert!(!ctx.clustered_leaf_hint_present());
}

#[test]
fn leaf_hint_cleared_by_invalidate_all() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    ctx.invalidate_all();
    assert!(
        !ctx.clustered_leaf_hint_present(),
        "invalidate_all must clear the hint"
    );
}

#[test]
fn leaf_hint_slot_returns_mutable_reference() {
    // The slot accessor lets storage::lookup_with_hint update the hint
    // in place. Verify the &mut Option<...> path works end to end.
    let mut ctx = SessionContext::default();
    {
        let slot = ctx.clustered_leaf_hint_slot();
        *slot = Some(fake_hint(7, 77, 777, 1));
    }
    assert!(ctx.clustered_leaf_hint_present());
    let h = ctx
        .get_clustered_leaf_hint(7, 77, 1, &[5u8])
        .expect("hint set via slot");
    assert_eq!(h.leaf_page_id, 777);
}
