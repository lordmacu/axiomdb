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
        /// True when this single-key lookup reproduces the ENTIRE WHERE
        /// predicate (a bare `col = literal` on a single-column index), so the
        /// executor may skip the per-row WHERE re-evaluation (SQLite
        /// `disableTerm`/`TERM_CODED`). Only the SELECT planner's exact-equality
        /// rule sets this; DELETE/UPDATE leave it `false` (they always recheck).
        covers_predicate: bool,
    },

    /// Range scan: iterate over B-Tree entries between `lo` and `hi`
    /// (`None` means unbounded). Strictness is carried explicitly so an
    /// exclusive `<`/`>` bound is honored exactly (SQLite `OP_SeekGT`/`SeekLT`)
    /// instead of being widened to inclusive and re-filtered per row.
    IndexRange {
        /// The index to use.
        index_def: IndexDef,
        /// Lower bound key (already encoded).
        lo: Option<Vec<u8>>,
        /// Upper bound key (already encoded).
        hi: Option<Vec<u8>>,
        /// Lower bound is inclusive (`>=`) when true, exclusive (`>`) when false.
        lo_inclusive: bool,
        /// Upper bound is inclusive (`<=`) when true, exclusive (`<`) when false.
        hi_inclusive: bool,
        /// True when these bounds reproduce the ENTIRE WHERE predicate, so the
        /// executor may skip the per-row WHERE re-evaluation (SQLite
        /// `disableTerm`/`TERM_CODED`). Only the pure-range SELECT planner rules
        /// set this; everything else leaves it `false` (conservative).
        covers_predicate: bool,
    },

    /// Index-only scan: all needed columns are in the index key columns or
    /// INCLUDE payload (Phase 13.5).
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
        /// Number of INCLUDE columns physically stored after the key bytes.
        n_include_cols: usize,
        /// For each needed SELECT column (in output order): the position within
        /// the decoded key values (0 = first key column, 1 = second, …).
        needed_key_positions: Vec<usize>,
    },

    /// GIN inverted index scan for JSONB containment (`col @> literal`) — Phase 11.17.
    ///
    /// The executor performs one B-Tree range scan per query term, intersects all
    /// resulting RID sets (AND semantics), and rechecks each candidate row via the
    /// full `@>` evaluator to eliminate false positives.
    ///
    /// B-Tree key format: `[term_bytes][0x00][page_id: 8 LE][slot_id: 2 LE]`.
    GinScan {
        /// The GIN index to use.
        index_def: IndexDef,
        /// Pre-extracted query terms from the right-hand JSONB literal.
        /// Each entry is `[GIN_FLAG_*][payload]` without the 0x00 separator or RID suffix.
        query_terms: Vec<Vec<u8>>,
    },
}
