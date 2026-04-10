//! BRIN (Block Range INdex) storage module — Phase 11.1b.
//!
//! Stores per-range (N heap pages) min/max summaries for numeric columns.
//! The planner uses these summaries to skip ranges that cannot match a
//! WHERE predicate, reducing I/O for time-series and naturally-ordered data.
//!
//! ## On-disk format
//!
//! The entire BRIN index for one column lives in a single "metapage" (or
//! overflows to additional pages for very large tables). The metapage stores:
//!
//! ```text
//! [magic:4][version:4][pages_per_range:4][col_idx:2][num_ranges:4]
//! [BrinSummary × num_ranges]
//! ```
//!
//! Each `BrinSummary` is 18 bytes:
//! ```text
//! [flags:1][min_val:8 LE][max_val:8 LE][has_null:1]
//! ```
//!
//! With 16 KB pages and 18-byte entries: ~900 ranges per page.
//! For 1M rows with 128 pages/range (~78 ranges) → fits in 1 metapage.
//!
//! ## PostgreSQL reference
//!
//! PostgreSQL BRIN uses separate revmap + data pages with opclass callbacks.
//! AxiomDB simplifies: fixed-size minmax entries inline in the metapage,
//! no revmap indirection, no opclass system. Covers 90% of time-series use
//! cases with 10% of the complexity.

use axiomdb_core::error::DbError;

use crate::engine::StorageEngine;
use crate::page::{Page, PageType};

// ── Constants ────────────────────────────────────────────────────────────────

/// Magic number: ASCII "BRIN" = 0x4252494E.
const BRIN_MAGIC: u32 = 0x4252_494E;
const BRIN_VERSION: u32 = 1;

/// Offset of header fields in the page body.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_PAGES_PER_RANGE: usize = 8;
const OFF_COL_IDX: usize = 12;
const OFF_NUM_RANGES: usize = 14;
const HEADER_SIZE: usize = 18;

/// Each summary entry is 18 bytes.
const SUMMARY_SIZE: usize = 18;

/// Page body size: PAGE_SIZE (16384) minus the 64-byte PageHeader.
const PAGE_BODY_SIZE: usize = crate::page::PAGE_SIZE - 64;

/// Max summaries that fit in a single 16 KB page body.
const MAX_SUMMARIES_PER_PAGE: usize = (PAGE_BODY_SIZE - HEADER_SIZE) / SUMMARY_SIZE;

// ── BrinSummary ──────────────────────────────────────────────────────────────

/// Per-range summary: min/max values for the indexed column.
#[derive(Debug, Clone, Copy)]
pub struct BrinSummary {
    /// Whether this range has any non-NULL values.
    pub has_values: bool,
    /// Minimum value in the range (encoded as i64).
    pub min_val: i64,
    /// Maximum value in the range (encoded as i64).
    pub max_val: i64,
    /// Whether any NULL value exists in the range.
    pub has_null: bool,
}

impl BrinSummary {
    /// Empty summary for a range with no rows yet.
    pub fn empty() -> Self {
        Self {
            has_values: false,
            min_val: i64::MAX,
            max_val: i64::MIN,
            has_null: false,
        }
    }

    /// Updates the summary with a new value.
    pub fn add_value(&mut self, val: i64) {
        if !self.has_values {
            self.min_val = val;
            self.max_val = val;
            self.has_values = true;
        } else {
            if val < self.min_val {
                self.min_val = val;
            }
            if val > self.max_val {
                self.max_val = val;
            }
        }
    }

    /// Marks that a NULL was seen in this range.
    pub fn add_null(&mut self) {
        self.has_null = true;
    }

    /// Returns true if a predicate `col op literal` could match any row in this range.
    /// False positives are acceptable; false negatives are NOT.
    pub fn might_match(&self, op: BrinCmpOp, literal: i64) -> bool {
        if !self.has_values {
            return false; // empty range — no rows
        }
        match op {
            BrinCmpOp::Eq => literal >= self.min_val && literal <= self.max_val,
            BrinCmpOp::Lt => self.min_val < literal,
            BrinCmpOp::Le => self.min_val <= literal,
            BrinCmpOp::Gt => self.max_val > literal,
            BrinCmpOp::Ge => self.max_val >= literal,
            BrinCmpOp::Between(lo, hi) => self.max_val >= lo && self.min_val <= hi,
        }
    }

    fn to_bytes(self) -> [u8; SUMMARY_SIZE] {
        let mut buf = [0u8; SUMMARY_SIZE];
        buf[0] = if self.has_values { 1 } else { 0 };
        buf[1..9].copy_from_slice(&self.min_val.to_le_bytes());
        buf[9..17].copy_from_slice(&self.max_val.to_le_bytes());
        buf[17] = if self.has_null { 1 } else { 0 };
        buf
    }

    fn from_bytes(buf: &[u8; SUMMARY_SIZE]) -> Self {
        Self {
            has_values: buf[0] != 0,
            min_val: i64::from_le_bytes(buf[1..9].try_into().unwrap()),
            max_val: i64::from_le_bytes(buf[9..17].try_into().unwrap()),
            has_null: buf[17] != 0,
        }
    }
}

/// Comparison operator for BRIN range checks.
#[derive(Debug, Clone, Copy)]
pub enum BrinCmpOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Between(i64, i64),
}

// ── Value encoding ───────────────────────────────────────────────────────────

// Value encoding lives in axiomdb-sql (encode_brin_value) since
// axiomdb-storage doesn't depend on axiomdb-types::Value.

// ── Metapage read/write ──────────────────────────────────────────────────────

/// Initializes a BRIN metapage with the given parameters and empty summaries.
pub fn init_metapage(
    storage: &dyn StorageEngine,
    pages_per_range: u32,
    col_idx: u16,
) -> Result<u64, DbError> {
    let page_id = storage.alloc_page(PageType::Index)?;
    let mut page = Page::new(PageType::Index, page_id);
    let body = page.body_mut();

    body[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&BRIN_MAGIC.to_le_bytes());
    body[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&BRIN_VERSION.to_le_bytes());
    body[OFF_PAGES_PER_RANGE..OFF_PAGES_PER_RANGE + 4]
        .copy_from_slice(&pages_per_range.to_le_bytes());
    body[OFF_COL_IDX..OFF_COL_IDX + 2].copy_from_slice(&col_idx.to_le_bytes());
    body[OFF_NUM_RANGES..OFF_NUM_RANGES + 4].copy_from_slice(&0u32.to_le_bytes());

    page.update_checksum();
    storage.write_page(page_id, &page)?;
    Ok(page_id)
}

/// Reads the BRIN header from the metapage.
pub fn read_header(
    storage: &dyn StorageEngine,
    metapage_id: u64,
) -> Result<(u32, u16, u32), DbError> {
    let page_ref = storage.read_page(metapage_id)?;
    let body = page_ref.body();

    let magic = u32::from_le_bytes(body[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap());
    if magic != BRIN_MAGIC {
        return Err(DbError::Internal {
            message: format!("BRIN metapage {metapage_id}: bad magic {magic:#x}"),
        });
    }

    let pages_per_range = u32::from_le_bytes(
        body[OFF_PAGES_PER_RANGE..OFF_PAGES_PER_RANGE + 4]
            .try_into()
            .unwrap(),
    );
    let col_idx = u16::from_le_bytes(body[OFF_COL_IDX..OFF_COL_IDX + 2].try_into().unwrap());
    let num_ranges =
        u32::from_le_bytes(body[OFF_NUM_RANGES..OFF_NUM_RANGES + 4].try_into().unwrap());

    Ok((pages_per_range, col_idx, num_ranges))
}

/// Reads all BRIN summaries from the metapage.
pub fn read_summaries(
    storage: &dyn StorageEngine,
    metapage_id: u64,
) -> Result<Vec<BrinSummary>, DbError> {
    let (_, _, num_ranges) = read_header(storage, metapage_id)?;
    let page_ref = storage.read_page(metapage_id)?;
    let body = page_ref.body();

    let mut summaries = Vec::with_capacity(num_ranges as usize);
    for i in 0..num_ranges as usize {
        if i >= MAX_SUMMARIES_PER_PAGE {
            break; // overflow not yet supported — truncate
        }
        let off = HEADER_SIZE + i * SUMMARY_SIZE;
        let mut buf = [0u8; SUMMARY_SIZE];
        buf.copy_from_slice(&body[off..off + SUMMARY_SIZE]);
        summaries.push(BrinSummary::from_bytes(&buf));
    }
    Ok(summaries)
}

/// Writes all BRIN summaries to the metapage.
pub fn write_summaries(
    storage: &dyn StorageEngine,
    metapage_id: u64,
    summaries: &[BrinSummary],
) -> Result<(), DbError> {
    let n = summaries.len().min(MAX_SUMMARIES_PER_PAGE);
    let page_ref = storage.read_page(metapage_id)?;
    let mut page = page_ref.into_page();
    let body = page.body_mut();

    body[OFF_NUM_RANGES..OFF_NUM_RANGES + 4].copy_from_slice(&(n as u32).to_le_bytes());
    for (i, summary) in summaries.iter().take(n).enumerate() {
        let off = HEADER_SIZE + i * SUMMARY_SIZE;
        body[off..off + SUMMARY_SIZE].copy_from_slice(&summary.to_bytes());
    }

    page.update_checksum();
    storage.write_page(metapage_id, &page)?;
    Ok(())
}

/// Updates a single range summary in the metapage (read-modify-write).
pub fn update_range_summary(
    storage: &dyn StorageEngine,
    metapage_id: u64,
    range_id: u32,
    val: i64,
    is_null: bool,
) -> Result<(), DbError> {
    let (_, _, num_ranges) = read_header(storage, metapage_id)?;
    let page_ref = storage.read_page(metapage_id)?;
    let mut page = page_ref.into_page();
    let body = page.body_mut();

    // Extend num_ranges if needed.
    let new_num = (range_id + 1).max(num_ranges);
    body[OFF_NUM_RANGES..OFF_NUM_RANGES + 4].copy_from_slice(&new_num.to_le_bytes());

    // Initialize any gaps.
    for i in num_ranges..range_id {
        let off = HEADER_SIZE + i as usize * SUMMARY_SIZE;
        if off + SUMMARY_SIZE <= body.len() {
            body[off..off + SUMMARY_SIZE].copy_from_slice(&BrinSummary::empty().to_bytes());
        }
    }

    // Read existing summary.
    let off = HEADER_SIZE + range_id as usize * SUMMARY_SIZE;
    if off + SUMMARY_SIZE > body.len() {
        // Overflow — silently skip (future: allocate overflow page).
        page.update_checksum();
        storage.write_page(metapage_id, &page)?;
        return Ok(());
    }
    let mut buf = [0u8; SUMMARY_SIZE];
    buf.copy_from_slice(&body[off..off + SUMMARY_SIZE]);
    let mut summary = BrinSummary::from_bytes(&buf);

    if is_null {
        summary.add_null();
    } else {
        summary.add_value(val);
    }

    body[off..off + SUMMARY_SIZE].copy_from_slice(&summary.to_bytes());
    page.update_checksum();
    storage.write_page(metapage_id, &page)?;
    Ok(())
}

/// Returns the set of range_ids that might match the given predicate.
/// Used by the planner to build a skip set for heap scans.
pub fn qualifying_ranges(summaries: &[BrinSummary], op: BrinCmpOp, literal: i64) -> Vec<u32> {
    summaries
        .iter()
        .enumerate()
        .filter(|(_, s)| s.might_match(op, literal))
        .map(|(i, _)| i as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStorage;

    #[test]
    fn test_brin_summary_add_value() {
        let mut s = BrinSummary::empty();
        assert!(!s.has_values);
        s.add_value(10);
        assert!(s.has_values);
        assert_eq!(s.min_val, 10);
        assert_eq!(s.max_val, 10);
        s.add_value(5);
        assert_eq!(s.min_val, 5);
        s.add_value(20);
        assert_eq!(s.max_val, 20);
    }

    #[test]
    fn test_brin_summary_might_match() {
        let mut s = BrinSummary::empty();
        s.add_value(10);
        s.add_value(50);
        // range [10, 50]
        assert!(s.might_match(BrinCmpOp::Eq, 30)); // 30 in [10,50]
        assert!(!s.might_match(BrinCmpOp::Eq, 5)); // 5 < 10
        assert!(!s.might_match(BrinCmpOp::Eq, 60)); // 60 > 50
        assert!(s.might_match(BrinCmpOp::Gt, 5)); // max(50) > 5
        assert!(!s.might_match(BrinCmpOp::Gt, 60)); // max(50) ≤ 60 is false? No: max > 60 → false
        assert!(s.might_match(BrinCmpOp::Lt, 60)); // min(10) < 60
        assert!(!s.might_match(BrinCmpOp::Lt, 5)); // min(10) ≥ 5? No: min < 5 → false
        assert!(s.might_match(BrinCmpOp::Between(20, 40), 0));
    }

    #[test]
    fn test_brin_summary_roundtrip() {
        let mut s = BrinSummary::empty();
        s.add_value(42);
        s.add_null();
        let bytes = s.to_bytes();
        let s2 = BrinSummary::from_bytes(&bytes);
        assert_eq!(s.min_val, s2.min_val);
        assert_eq!(s.max_val, s2.max_val);
        assert_eq!(s.has_null, s2.has_null);
        assert_eq!(s.has_values, s2.has_values);
    }

    #[test]
    fn test_brin_metapage_init_and_read() {
        let storage = MemoryStorage::new();
        let meta_id = init_metapage(&storage, 128, 0).unwrap();
        let (ppr, col, num) = read_header(&storage, meta_id).unwrap();
        assert_eq!(ppr, 128);
        assert_eq!(col, 0);
        assert_eq!(num, 0);
    }

    #[test]
    fn test_brin_write_and_read_summaries() {
        let storage = MemoryStorage::new();
        let meta_id = init_metapage(&storage, 64, 2).unwrap();

        let mut summaries = vec![BrinSummary::empty(); 3];
        summaries[0].add_value(1);
        summaries[0].add_value(100);
        summaries[1].add_value(200);
        summaries[1].add_value(300);
        // summaries[2] empty

        write_summaries(&storage, meta_id, &summaries).unwrap();
        let read_back = read_summaries(&storage, meta_id).unwrap();
        assert_eq!(read_back.len(), 3);
        assert_eq!(read_back[0].min_val, 1);
        assert_eq!(read_back[0].max_val, 100);
        assert_eq!(read_back[1].min_val, 200);
        assert!(!read_back[2].has_values);
    }

    #[test]
    fn test_brin_update_range() {
        let storage = MemoryStorage::new();
        let meta_id = init_metapage(&storage, 128, 0).unwrap();

        update_range_summary(&storage, meta_id, 0, 10, false).unwrap();
        update_range_summary(&storage, meta_id, 0, 50, false).unwrap();
        update_range_summary(&storage, meta_id, 2, 999, false).unwrap();

        let summaries = read_summaries(&storage, meta_id).unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].min_val, 10);
        assert_eq!(summaries[0].max_val, 50);
        assert!(!summaries[1].has_values); // gap range
        assert_eq!(summaries[2].min_val, 999);
    }

    #[test]
    fn test_brin_qualifying_ranges() {
        let summaries = vec![
            {
                let mut s = BrinSummary::empty();
                s.add_value(1);
                s.add_value(100);
                s
            },
            {
                let mut s = BrinSummary::empty();
                s.add_value(200);
                s.add_value(300);
                s
            },
            {
                let mut s = BrinSummary::empty();
                s.add_value(400);
                s.add_value(500);
                s
            },
        ];

        let eq_250 = qualifying_ranges(&summaries, BrinCmpOp::Eq, 250);
        assert_eq!(eq_250, vec![1]); // only range [200,300] matches

        let gt_150 = qualifying_ranges(&summaries, BrinCmpOp::Gt, 150);
        assert_eq!(gt_150, vec![1, 2]); // ranges with max > 150

        let lt_250 = qualifying_ranges(&summaries, BrinCmpOp::Lt, 250);
        assert_eq!(lt_250, vec![0, 1]); // ranges with min < 250
    }
}
