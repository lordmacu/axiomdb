//! SQL expression tree — the AST node type for all SQL expressions.
//!
//! [`Expr`] is produced by the SQL parser (Phase 4.1–4.4) and consumed by
//! the expression evaluator ([`eval`]). Column references are resolved to
//! `col_idx` positions by the semantic analyzer (Phase 4.18).
//!
//! [`eval`]: crate::eval::eval

use axiomdb_types::Value;

use crate::ast::SortOrder;

// ── Expr ──────────────────────────────────────────────────────────────────────

/// A SQL expression tree node.
///
/// Every `Expr` evaluates to a [`Value`] via [`eval`]. `Value::Null` is used
/// to represent SQL UNKNOWN in boolean contexts (3-valued logic).
///
/// [`eval`]: crate::eval::eval
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    // ── Terminals ─────────────────────────────────────────────────────────────
    /// A literal SQL value: `42`, `'hello'`, `TRUE`, `NULL`, `3.14`, etc.
    Literal(Value),

    /// A column reference resolved to a row-slice index by the semantic
    /// analyzer (Phase 4.18). `col_idx` is the position in the `&[Value]`
    /// row passed to `eval`. `name` is preserved for error messages.
    Column { col_idx: usize, name: String },

    // ── Unary operators ───────────────────────────────────────────────────────
    /// A unary operator applied to one operand.
    UnaryOp { op: UnaryOp, operand: Box<Expr> },

    // ── Binary operators ──────────────────────────────────────────────────────
    /// A binary operator applied to two operands.
    ///
    /// `AND` and `OR` use short-circuit evaluation: the right operand is not
    /// evaluated when the result is already determined by the left.
    BinaryOp {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },

    // ── Special SQL forms ─────────────────────────────────────────────────────
    /// `expr IS [NOT] NULL`
    ///
    /// Unlike comparisons, `IS NULL` does not propagate NULL: it always returns
    /// `TRUE` or `FALSE`.
    IsNull { expr: Box<Expr>, negated: bool },

    /// `expr [NOT] BETWEEN low AND high`
    ///
    /// Semantically equivalent to `expr >= low AND expr <= high` with full
    /// NULL propagation through both comparisons.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },

    /// `expr [NOT] LIKE pattern`
    ///
    /// `%` = any sequence of zero or more characters.
    /// `_` = exactly one character.
    /// Case-sensitive. Operates on Unicode characters (not bytes).
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },

    /// `expr [NOT] IN (v1, v2, ...)`
    ///
    /// Short-circuits to `TRUE` on first match. If no match is found and the
    /// list contains `NULL`, the result is `NULL` (UNKNOWN). If no match and
    /// no `NULL` in list, the result is `FALSE`.
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },

    // ── Function call ─────────────────────────────────────────────────────────
    /// `func_name(arg1, arg2, ...)`
    ///
    /// Function implementations are registered in Phase 4.19. Evaluating an
    /// unregistered function returns `DbError::NotImplemented`.
    Function { name: String, args: Vec<Expr> },

    // ── CASE WHEN ─────────────────────────────────────────────────────────────
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`
    ///
    /// ## Searched CASE (`operand = None`)
    ///
    /// Each `when_thens.0` is a boolean condition evaluated with `is_truthy`.
    /// The first truthy branch wins; the rest are not evaluated.
    ///
    /// ```sql
    /// CASE WHEN salary > 100000 THEN 'senior' ELSE 'junior' END
    /// ```
    ///
    /// ## Simple CASE (`operand = Some(base)`)
    ///
    /// Each `when_thens.0` is compared to `base` using SQL equality semantics.
    /// `NULL base` or `NULL value` produces UNKNOWN → no match.
    ///
    /// ```sql
    /// CASE status WHEN 'active' THEN 1 WHEN 'inactive' THEN 0 ELSE -1 END
    /// ```
    ///
    /// ## No match
    ///
    /// If no WHEN branch matches and `else_result` is `None`, the expression
    /// evaluates to `NULL`.
    Case {
        /// `None` for searched CASE; `Some(base_expr)` for simple CASE.
        operand: Option<Box<Expr>>,
        /// `(condition_or_value, result)` pairs evaluated left-to-right.
        when_thens: Vec<(Expr, Expr)>,
        /// Optional ELSE result. Returns `NULL` if absent and no WHEN matched.
        else_result: Option<Box<Expr>>,
    },

    /// `CAST(expr AS type)` — explicit type conversion.
    ///
    /// Evaluates `expr` and coerces the result to `target` using strict mode.
    /// `NULL` always returns `NULL`. Invalid conversions return
    /// [`DbError::InvalidCoercion`].
    Cast {
        /// The expression to convert.
        expr: Box<Expr>,
        /// The target SQL type.
        target: axiomdb_types::DataType,
    },

    // ── Subqueries ────────────────────────────────────────────────────────────
    /// `(SELECT expr FROM ...)` — scalar subquery.
    ///
    /// Must return exactly one column. Returns `NULL` if 0 rows are produced.
    /// Returns [`DbError::CardinalityViolation`] if more than one row is produced.
    Subquery(Box<crate::ast::SelectStmt>),

    /// `expr [NOT] IN (SELECT col FROM ...)` — membership test against a subquery.
    ///
    /// NULL semantics follow SQL `IN`:
    /// - `expr` is NULL → NULL
    /// - match found → TRUE
    /// - no match, no NULLs in result → FALSE
    /// - no match, at least one NULL in result → NULL (UNKNOWN)
    InSubquery {
        expr: Box<Expr>,
        query: Box<crate::ast::SelectStmt>,
        negated: bool,
    },

    /// `[NOT] EXISTS (SELECT ...)` — existence test.
    ///
    /// TRUE if the subquery returns at least one row, FALSE otherwise.
    /// Never returns NULL — EXISTS is immune to NULL propagation.
    Exists {
        query: Box<crate::ast::SelectStmt>,
        negated: bool,
    },

    /// A column reference resolved to the **outer** query's row.
    ///
    /// Emitted by the semantic analyzer when a column name is not found in the
    /// inner (current) scope but IS found in an enclosing scope.
    /// `col_idx` is the position in the outer row as resolved by the outer
    /// [`BindContext`].
    ///
    /// Must be replaced with [`Expr::Literal`] via `substitute_outer` before
    /// the inner query is executed. Reaching `eval_with` with an unsubstituted
    /// `OuterColumn` is a programming error.
    ///
    /// [`BindContext`]: crate::analyzer::BindContext
    OuterColumn { col_idx: usize, name: String },

    /// A positional parameter placeholder — the `?` in a prepared statement.
    ///
    /// `idx` is the 0-based index of the parameter (first `?` → 0, second → 1).
    ///
    /// Produced by the parser when parsing a prepared statement SQL template.
    /// Must be replaced with `Expr::Literal(value)` via `substitute_params_in_ast`
    /// before the statement is executed. Reaching `eval_with` with an
    /// unsubstituted `Param` is a programming error.
    Param { idx: usize },

    // ── Aggregate-specific forms ───────────────────────────────────────────────
    /// `GROUP_CONCAT([DISTINCT] expr [ORDER BY e [ASC|DESC], ...] [SEPARATOR 'str'])`
    ///
    /// Concatenates values from a group into a single text string.
    /// MySQL-compatible syntax; `string_agg(expr, sep)` is an alias (PostgreSQL form).
    ///
    /// - NULL values are silently skipped.
    /// - Empty group (or all-NULL group) returns `Value::Null`.
    /// - Result is truncated at 1,048,576 bytes (`group_concat_max_len`).
    ///
    /// Only valid as an aggregate in a grouped or implicitly-grouped SELECT.
    /// Reaching `eval()` with this variant (outside aggregate context) returns
    /// `DbError::InvalidValue`.
    GroupConcat {
        /// The expression to evaluate and concatenate per row (coerced to TEXT).
        expr: Box<Expr>,
        /// If true, duplicate values are removed before concatenation.
        distinct: bool,
        /// Optional per-aggregate ORDER BY: list of `(sort_expr, direction)`.
        /// Values are sorted by these keys before concatenation.
        order_by: Vec<(Expr, SortOrder)>,
        /// String placed between consecutive values. Default: `","`.
        separator: String,
    },
}

// ── BinaryOp ──────────────────────────────────────────────────────────────────

/// Binary operator for [`Expr::BinaryOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // Arithmetic
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` — integer division truncates toward zero; division by zero is an error.
    Div,
    /// `%` — modulo; division by zero is an error.
    Mod,

    // Comparison
    /// `=`
    Eq,
    /// `<>` or `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,

    // Boolean
    /// `AND` — short-circuit: `FALSE AND anything = FALSE`.
    And,
    /// `OR` — short-circuit: `TRUE OR anything = TRUE`.
    Or,

    // String
    /// `||` — string concatenation. Both operands must be `Text`.
    Concat,
}

// ── UnaryOp ───────────────────────────────────────────────────────────────────

/// Unary operator for [`Expr::UnaryOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation: `-expr`.
    Neg,
    /// Boolean negation: `NOT expr`.
    Not,
}

// ── Convenience constructors ──────────────────────────────────────────────────

impl Expr {
    /// Shorthand: `Expr::Literal(Value::Null)`.
    pub fn null() -> Self {
        Self::Literal(Value::Null)
    }

    /// Shorthand: `Expr::Literal(Value::Bool(b))`.
    pub fn bool(b: bool) -> Self {
        Self::Literal(Value::Bool(b))
    }

    /// Shorthand: `Expr::Literal(Value::Int(n))`.
    pub fn int(n: i32) -> Self {
        Self::Literal(Value::Int(n))
    }

    /// Shorthand: `Expr::Literal(Value::Text(s.into()))`.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Literal(Value::Text(s.into()))
    }

    /// Shorthand: binary op between two expressions.
    pub fn binop(op: BinaryOp, left: Expr, right: Expr) -> Self {
        Self::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }
}
