//! Per-page zone map — min/max metadata for scan skip (Phase 8.3b).
//!
//! Stores min/max values for one numeric column in `PageHeader._reserved[8..26]`.
//! During full scans, the zone map is checked BEFORE decoding any rows on the
//! page. If the predicate is entirely outside [min, max], the entire page is
//! skipped — eliminating all per-row decode + visibility check overhead.
//!
//! ## Layout (in _reserved[8..26])
//!
//! ```text
//! Offset 8:   version (u8) — 0 = no zone map, 1 = active
//! Offset 9:   col_idx (u8) — which column (0-255)
//! Offset 10:  min_value (i64 LE) — minimum value on this page
//! Offset 18:  max_value (i64 LE) — maximum value on this page
//! ```
//!
//! ## Design reference
//!
//! - DuckDB: per-segment min/max, skips 2048-row vectors
//! - PostgreSQL BRIN: per-128-pages min/max
//! - OceanBase: per-micro-block min/max + null_count

use crate::page::Page;

/// Offset of zone map data within PageHeader._reserved.
const ZM_OFFSET: usize = 8; // _reserved[8..26], _reserved[0..8] = next_page_id
const ZM_VERSION_OFFSET: usize = ZM_OFFSET;
const ZM_COL_OFFSET: usize = ZM_OFFSET + 1;
const ZM_MIN_OFFSET: usize = ZM_OFFSET + 2;
const ZM_MAX_OFFSET: usize = ZM_OFFSET + 10;

/// Active zone map version.
const ZM_VERSION_1: u8 = 1;

/// Per-page zone map for one numeric column.
#[derive(Debug, Clone, Copy)]
pub struct ZoneMap {
    pub col_idx: u8,
    pub min_val: i64,
    pub max_val: i64,
}

/// Reads the zone map from a page header. Returns None if no zone map is set.
pub fn read_zone_map(page: &Page) -> Option<ZoneMap> {
    let reserved = &page.header()._reserved;
    if reserved[ZM_VERSION_OFFSET] != ZM_VERSION_1 {
        return None;
    }
    let col_idx = reserved[ZM_COL_OFFSET];
    let min_val = i64::from_le_bytes(
        reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    let max_val = i64::from_le_bytes(
        reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    Some(ZoneMap {
        col_idx,
        min_val,
        max_val,
    })
}

/// Writes a zone map to a page header.
pub fn write_zone_map(page: &mut Page, zm: &ZoneMap) {
    let reserved = &mut page.header_mut()._reserved;
    reserved[ZM_VERSION_OFFSET] = ZM_VERSION_1;
    reserved[ZM_COL_OFFSET] = zm.col_idx;
    reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8].copy_from_slice(&zm.min_val.to_le_bytes());
    reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8].copy_from_slice(&zm.max_val.to_le_bytes());
}

/// Clears the zone map (sets version=0). Called on UPDATE to invalidate.
pub fn clear_zone_map(page: &mut Page) {
    page.header_mut()._reserved[ZM_VERSION_OFFSET] = 0;
}

/// Updates the zone map with a new value. If no zone map exists, initializes it.
/// If the value extends min or max, updates accordingly.
pub fn update_zone_map(page: &mut Page, col_idx: u8, value: i64) {
    // Check existing slot count BEFORE taking the mutable borrow on header.
    let existing_slots = crate::heap::num_slots(page);
    let reserved = &mut page.header_mut()._reserved;
    if reserved[ZM_VERSION_OFFSET] != ZM_VERSION_1 || reserved[ZM_COL_OFFSET] != col_idx {
        // Initialize new zone map for this column.
        reserved[ZM_VERSION_OFFSET] = ZM_VERSION_1;
        reserved[ZM_COL_OFFSET] = col_idx;
        // The page may already contain rows from non-zone-map inserts.
        // If so, use the widest possible bounds to avoid falsely skipping.
        // For a fresh/empty page, use the exact value.
        //
        // Note: existing_slots counts ALL slots including the one just inserted
        // (insert_tuple runs before update_zone_map). So >1 means pre-existing rows.
        if existing_slots > 1 {
            reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8].copy_from_slice(&i64::MIN.to_le_bytes());
            reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8].copy_from_slice(&i64::MAX.to_le_bytes());
        } else {
            reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8].copy_from_slice(&value.to_le_bytes());
            reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8].copy_from_slice(&value.to_le_bytes());
        }
        return;
    }
    // Update existing zone map.
    let cur_min = i64::from_le_bytes(
        reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    let cur_max = i64::from_le_bytes(
        reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    if value < cur_min {
        reserved[ZM_MIN_OFFSET..ZM_MIN_OFFSET + 8].copy_from_slice(&value.to_le_bytes());
    }
    if value > cur_max {
        reserved[ZM_MAX_OFFSET..ZM_MAX_OFFSET + 8].copy_from_slice(&value.to_le_bytes());
    }
}

/// Checks if a predicate might match rows on this page.
///
/// Returns `true` if rows MIGHT match (must scan the page).
/// Returns `false` if rows DEFINITELY don't match (skip the page).
///
/// Predicate types:
/// - `Eq(val)`: matches if `min <= val <= max`
/// - `Gt(val)`: matches if `max > val`
/// - `GtEq(val)`: matches if `max >= val`
/// - `Lt(val)`: matches if `min < val`
/// - `LtEq(val)`: matches if `min <= val`
/// - `Between(lo, hi)`: matches if `min <= hi && max >= lo`
pub fn zone_map_might_match(zm: &ZoneMap, predicate: &ZoneMapPredicate) -> bool {
    match predicate {
        ZoneMapPredicate::Eq(val) => zm.min_val <= *val && *val <= zm.max_val,
        ZoneMapPredicate::Gt(val) => zm.max_val > *val,
        ZoneMapPredicate::GtEq(val) => zm.max_val >= *val,
        ZoneMapPredicate::Lt(val) => zm.min_val < *val,
        ZoneMapPredicate::LtEq(val) => zm.min_val <= *val,
        ZoneMapPredicate::Between(lo, hi) => zm.min_val <= *hi && zm.max_val >= *lo,
    }
}

/// Predicate that can be checked against a zone map.
#[derive(Debug, Clone)]
pub enum ZoneMapPredicate {
    Eq(i64),
    Gt(i64),
    GtEq(i64),
    Lt(i64),
    LtEq(i64),
    Between(i64, i64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Page, PageType};

    #[test]
    fn test_write_read_zone_map() {
        let mut page = Page::new(PageType::Data, 5);
        let zm = ZoneMap {
            col_idx: 2,
            min_val: 10,
            max_val: 100,
        };
        write_zone_map(&mut page, &zm);

        let read = read_zone_map(&page).unwrap();
        assert_eq!(read.col_idx, 2);
        assert_eq!(read.min_val, 10);
        assert_eq!(read.max_val, 100);
    }

    #[test]
    fn test_no_zone_map_returns_none() {
        let page = Page::new(PageType::Data, 5);
        assert!(read_zone_map(&page).is_none());
    }

    #[test]
    fn test_update_extends_range() {
        let mut page = Page::new(PageType::Data, 5);
        update_zone_map(&mut page, 0, 50);
        update_zone_map(&mut page, 0, 10);
        update_zone_map(&mut page, 0, 90);

        let zm = read_zone_map(&page).unwrap();
        assert_eq!(zm.min_val, 10);
        assert_eq!(zm.max_val, 90);
    }

    #[test]
    fn test_clear_zone_map() {
        let mut page = Page::new(PageType::Data, 5);
        update_zone_map(&mut page, 0, 50);
        assert!(read_zone_map(&page).is_some());

        clear_zone_map(&mut page);
        assert!(read_zone_map(&page).is_none());
    }

    #[test]
    fn test_predicate_eq() {
        let zm = ZoneMap {
            col_idx: 0,
            min_val: 10,
            max_val: 100,
        };
        assert!(zone_map_might_match(&zm, &ZoneMapPredicate::Eq(50)));
        assert!(!zone_map_might_match(&zm, &ZoneMapPredicate::Eq(5)));
        assert!(!zone_map_might_match(&zm, &ZoneMapPredicate::Eq(101)));
    }

    #[test]
    fn test_predicate_range() {
        let zm = ZoneMap {
            col_idx: 0,
            min_val: 20,
            max_val: 80,
        };
        assert!(zone_map_might_match(&zm, &ZoneMapPredicate::Gt(19)));
        assert!(!zone_map_might_match(&zm, &ZoneMapPredicate::Gt(80)));
        assert!(zone_map_might_match(&zm, &ZoneMapPredicate::Lt(21)));
        assert!(!zone_map_might_match(&zm, &ZoneMapPredicate::Lt(20)));
        assert!(zone_map_might_match(
            &zm,
            &ZoneMapPredicate::Between(50, 60)
        ));
        assert!(!zone_map_might_match(
            &zm,
            &ZoneMapPredicate::Between(81, 90)
        ));
    }
}
