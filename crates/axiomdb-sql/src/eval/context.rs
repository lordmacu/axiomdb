use std::cell::Cell;

use axiomdb_core::error::DbError;

use crate::{ast::SelectStmt, result::QueryResult, session::SessionCollation};

// ── Session-collation thread-local ────────────────────────────────────────────

// Active text-comparison collation for the current `eval` call stack.
//
// Set to [`SessionCollation::Binary`] by default; overridden to `Es` for the
// duration of a ctx-path execution via [`CollationGuard`].
//
// Thread-local guarantees correct isolation between concurrent sessions even
// though `eval` / `eval_with` are non-async functions called from a Tokio
// spawn_blocking context.
thread_local! {
    static EVAL_COLLATION: Cell<SessionCollation> = const { Cell::new(SessionCollation::Binary) };
}

/// Returns the active session collation for the current thread.
///
/// Called by `compare_values` and the LIKE handler to dispatch between
/// binary and folded text semantics.
pub(crate) fn current_eval_collation() -> SessionCollation {
    EVAL_COLLATION.with(|c| c.get())
}

/// RAII guard that sets the thread-local session collation on construction
/// and restores the previous value on drop.
///
/// ```rust,ignore
/// let _guard = CollationGuard::new(ctx.effective_collation());
/// // All eval() / eval_with() calls here use the session collation.
/// ```
pub struct CollationGuard {
    prev: SessionCollation,
}

impl CollationGuard {
    /// Sets the active collation for the current thread.
    pub fn new(coll: SessionCollation) -> Self {
        let prev = EVAL_COLLATION.with(|c| c.get());
        EVAL_COLLATION.with(|c| c.set(coll));
        Self { prev }
    }
}

impl Drop for CollationGuard {
    fn drop(&mut self) {
        EVAL_COLLATION.with(|c| c.set(self.prev));
    }
}

// ── SubqueryRunner ────────────────────────────────────────────────────────────

/// Provides subquery execution to [`crate::eval::eval_with`].
///
/// Implement this trait for a type that can execute a [`SelectStmt`] and return
/// its result. Use [`NoSubquery`] where subqueries are impossible; the compiler
/// monomorphizes `eval_with::<NoSubquery>` and eliminates all subquery branches
/// at zero runtime cost.
pub trait SubqueryRunner {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError>;
}

/// Zero-cost runner for expression contexts that cannot contain subqueries.
///
/// `eval(expr, row)` uses this, so subquery `Expr` variants encountered by the
/// pure evaluator return `NotImplemented` instead of panicking.
pub struct NoSubquery;

impl SubqueryRunner for NoSubquery {
    fn run(&mut self, _: &SelectStmt) -> Result<QueryResult, DbError> {
        Err(DbError::NotImplemented {
            feature: "subquery in expression context — use eval_with instead of eval".into(),
        })
    }
}

/// Wrapper so a `FnMut` closure can be used as a [`SubqueryRunner`].
pub struct ClosureRunner<F>(pub F)
where
    F: FnMut(&SelectStmt) -> Result<QueryResult, DbError>;

impl<F> SubqueryRunner for ClosureRunner<F>
where
    F: FnMut(&SelectStmt) -> Result<QueryResult, DbError>,
{
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> {
        (self.0)(stmt)
    }
}
