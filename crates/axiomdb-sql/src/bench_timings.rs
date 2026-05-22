//! Thread-local timing accumulators for the INSERT path.
//!
//! Used ONLY by the `axiomdb_bench --diagnose-insert-deep` diagnostic.
//! Compiled away to zero cost when the `bench-timings` Cargo feature is off.
//!
//! ```text
//! # Enable for a single bench run:
//! cargo run -p axiomdb-bench-comparison --release \
//!     --features axiomdb-sql/bench-timings -- \
//!     --scenario insert_batch --rows 10000 --diagnose-insert-deep
//! ```

#[cfg(feature = "bench-timings")]
use std::cell::RefCell;

/// Per-phase nanosecond accumulators for the INSERT loop.
///
/// Each field records the cumulative time spent in that phase across all
/// rows of the current thread, so the bench can divide by row count to get
/// per-row averages.
#[cfg(feature = "bench-timings")]
#[derive(Default, Clone, Copy, Debug)]
pub struct InsertPhaseTimings {
    pub eval_ns: u128,
    pub validate_ns: u128,
    pub auto_inc_ns: u128,
    pub generated_cols_ns: u128,
    pub constraints_ns: u128,
    pub fk_check_ns: u128,
    pub prepare_row_ns: u128,
    pub enum_validate_ns: u128,
    pub pk_dup_ns: u128,
    pub batch_push_ns: u128,
    /// 6d: per-statement table resolve (execute_insert_ctx fast path / resolve_table_cached).
    pub resolve_ns: u128,
    pub rows: u64,
}

#[cfg(feature = "bench-timings")]
thread_local! {
    pub static INSERT_TIMINGS: RefCell<InsertPhaseTimings> =
        const { RefCell::new(InsertPhaseTimings {
            eval_ns: 0, validate_ns: 0, auto_inc_ns: 0, generated_cols_ns: 0,
            constraints_ns: 0, fk_check_ns: 0, prepare_row_ns: 0,
            enum_validate_ns: 0, pk_dup_ns: 0, batch_push_ns: 0,
            resolve_ns: 0, rows: 0,
        }) };
}

/// Resets all counters to zero. Call before a timed run.
#[cfg(feature = "bench-timings")]
pub fn reset_insert_timings() {
    INSERT_TIMINGS.with(|t| *t.borrow_mut() = InsertPhaseTimings::default());
}

/// Returns a snapshot of the current counters.
#[cfg(feature = "bench-timings")]
pub fn snapshot_insert_timings() -> InsertPhaseTimings {
    INSERT_TIMINGS.with(|t| *t.borrow())
}

/// Macro: time a block and add the elapsed ns to a named field.
/// Compiles to just the expression when `bench-timings` is off.
#[cfg(feature = "bench-timings")]
#[macro_export]
macro_rules! time_insert_phase {
    ($field:ident, $body:expr) => {{
        let __t0 = std::time::Instant::now();
        let __result = { $body };
        let __ns = __t0.elapsed().as_nanos();
        $crate::bench_timings::INSERT_TIMINGS.with(|t| {
            t.borrow_mut().$field += __ns;
        });
        __result
    }};
}

#[cfg(not(feature = "bench-timings"))]
#[macro_export]
macro_rules! time_insert_phase {
    ($field:ident, $body:expr) => {
        $body
    };
}

#[cfg(feature = "bench-timings")]
pub fn bump_rows(n: u64) {
    INSERT_TIMINGS.with(|t| t.borrow_mut().rows += n);
}

#[cfg(not(feature = "bench-timings"))]
pub fn bump_rows(_n: u64) {}

// ── SELECT / point-lookup phase timings (--diagnose-point) ────────────────────

/// Per-phase nanosecond accumulators for the prepared-SELECT execute path.
///
/// `clone_ns`: `substitute_params(self.analyzed.clone())`. `plan_ns`: the
/// `plan_select_ctx` planner run. `exec_ns`: the whole executor call (includes
/// `plan_ns`); so `exec_ns - plan_ns` ≈ lookup + decode + setup. Used to decide
/// whether the prepared-execute gap is removable (clone + plan) vs irreducible
/// (lookup + decode), i.e. whether the SQLite compiled-VDBE "B1" rework pays off.
#[cfg(feature = "bench-timings")]
#[derive(Default, Clone, Copy, Debug)]
pub struct SelectPhaseTimings {
    pub clone_ns: u128,
    pub resolve_ns: u128,
    pub stats_ns: u128,
    pub plan_ns: u128,
    pub lookup_ns: u128,
    pub where_ns: u128,
    pub colmeta_ns: u128,
    pub exec_ns: u128,
    pub calls: u64,
}

#[cfg(feature = "bench-timings")]
thread_local! {
    pub static SELECT_TIMINGS: RefCell<SelectPhaseTimings> =
        const { RefCell::new(SelectPhaseTimings {
            clone_ns: 0, resolve_ns: 0, stats_ns: 0, plan_ns: 0,
            lookup_ns: 0, where_ns: 0, colmeta_ns: 0, exec_ns: 0, calls: 0,
        }) };
}

#[cfg(feature = "bench-timings")]
pub fn reset_select_timings() {
    SELECT_TIMINGS.with(|t| *t.borrow_mut() = SelectPhaseTimings::default());
}

#[cfg(feature = "bench-timings")]
pub fn snapshot_select_timings() -> SelectPhaseTimings {
    SELECT_TIMINGS.with(|t| *t.borrow())
}

#[cfg(feature = "bench-timings")]
pub fn bump_select_calls(n: u64) {
    SELECT_TIMINGS.with(|t| t.borrow_mut().calls += n);
}

#[cfg(not(feature = "bench-timings"))]
pub fn bump_select_calls(_n: u64) {}

/// Macro: time a block and add the elapsed ns to a named `SelectPhaseTimings`
/// field. Compiles to just the expression when `bench-timings` is off.
#[cfg(feature = "bench-timings")]
#[macro_export]
macro_rules! time_select_phase {
    ($field:ident, $body:expr) => {{
        let __t0 = std::time::Instant::now();
        let __result = { $body };
        let __ns = __t0.elapsed().as_nanos();
        $crate::bench_timings::SELECT_TIMINGS.with(|t| {
            t.borrow_mut().$field += __ns;
        });
        __result
    }};
}

#[cfg(not(feature = "bench-timings"))]
#[macro_export]
macro_rules! time_select_phase {
    ($field:ident, $body:expr) => {
        $body
    };
}
