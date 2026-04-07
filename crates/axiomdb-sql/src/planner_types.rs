// ── Statistics cost constants (Phase 6.10) ───────────────────────────────────

/// Fraction of rows below which an index scan beats a full scan.
/// Derived from PostgreSQL: seq_page_cost=1 / random_page_cost=4 ≈ 0.25.
/// AxiomDB uses 0.20 (slightly conservative for embedded single-file storage).
const INDEX_SELECTIVITY_THRESHOLD: f64 = 0.20;

/// Fallback NDV when no statistics exist (PostgreSQL `DEFAULT_NUM_DISTINCT`).
const DEFAULT_NUM_DISTINCT: i64 = 200;

/// Tables with fewer than this many rows always use a full scan.
/// Index overhead dominates for very small tables.
const SMALL_TABLE_THRESHOLD: u64 = 100;

// ── AccessMethod ─────────────────────────────────────────────────────────────

/// The access method chosen by the planner for a single table scan.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessMethod {
    /// Full sequential scan — read every row from the heap.
    Scan,

    /// Point lookup: look up exactly one key in the B-Tree and read
    /// the corresponding heap row.
    IndexLookup {
        /// The index to use.
        index_def: IndexDef,
        /// Pre-encoded key bytes (via `encode_index_key`).
        key: Vec<u8>,
    },

    /// Range scan: iterate over B-Tree entries between `lo` and `hi`
    /// (both inclusive; `None` means unbounded).
    IndexRange {
        /// The index to use.
        index_def: IndexDef,
        /// Lower bound key (inclusive, already encoded).
        lo: Option<Vec<u8>>,
        /// Upper bound key (inclusive, already encoded).
        hi: Option<Vec<u8>>,
    },

    /// Index-only scan: all needed columns are in the index key columns (Phase 6.13).
    ///
    /// The executor decodes column values from the B-Tree key bytes instead of
    /// fetching the full heap row. A lightweight MVCC check (slot header only)
    /// is still performed on the heap to verify row visibility.
    IndexOnlyScan {
        /// The covering index.
        index_def: IndexDef,
        /// Lower bound key (inclusive). Equal to `hi` for point lookups.
        lo: Vec<u8>,
        /// Upper bound key (inclusive, `None` = unbounded; for point lookup: `Some(lo.clone())`).
        hi: Option<Vec<u8>>,
        /// Number of columns in the index key (= `index_def.columns.len()`).
        n_key_cols: usize,
        /// For each needed SELECT column (in output order): the position within
        /// the decoded key values (0 = first key column, 1 = second, …).
        needed_key_positions: Vec<usize>,
    },
}
