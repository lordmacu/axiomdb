//! SQL expression tree — the AST node type for all SQL expressions.
//!
//! [`Expr`] is produced by the SQL parser (Phase 4.1–4.4) and consumed by
//! the expression evaluator ([`eval`]). Column references are resolved to
//! `col_idx` positions by the semantic analyzer (Phase 4.18).
//!
//! [`eval`]: crate::eval::eval

use axiomdb_types::Value;

use crate::ast::{SortOrder, WindowSpec};

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

    /// `expr COLLATE name`
    ///
    /// Uses the named collation for the wrapped subtree when the executor
    /// performs text comparisons, LIKE matching, or other collation-aware
    /// operations.
    Collate { expr: Box<Expr>, collation: String },

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

    /// `expr [NOT] LIKE pattern [ESCAPE char]`
    ///
    /// `%` = any sequence of zero or more characters.
    /// `_` = exactly one character.
    /// `ESCAPE char` overrides the default backslash escape character.
    /// Case-sensitive. Operates on Unicode characters (not bytes).
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        /// Optional single-character escape override. `None` = no escape character.
        escape: Option<Box<Expr>>,
    },

    /// `expr IS [NOT] TRUE` / `expr IS [NOT] FALSE`
    ///
    /// Unlike `= TRUE`, this predicate never returns NULL — it always returns
    /// TRUE or FALSE. NULL IS TRUE → FALSE, NULL IS NOT TRUE → TRUE.
    IsBoolean {
        expr: Box<Expr>,
        value: bool,
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

    /// Bounded SQL window function support for ranking functions.
    ///
    /// Supported in Phase 13.2:
    /// - `ROW_NUMBER() OVER (...)`
    /// - `RANK() OVER (...)`
    /// - `DENSE_RANK() OVER (...)`
    ///
    /// Execution happens in a dedicated post-scan decoration phase, not via
    /// the scalar expression evaluator.
    Window { func: WindowFunc, spec: WindowSpec },

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
    ///
    /// `depth` is the nesting distance to the target scope: `0` = immediate
    /// enclosing query, `1` = grandparent, etc. Each substitution pass replaces
    /// nodes at depth 0 and decrements deeper nodes so the outer substitution
    /// frame picks them up. Supports correlation at any nesting depth (GAP-C.8).
    OuterColumn {
        col_idx: usize,
        name: String,
        depth: u16,
    },

    /// `VALUES(col)` inside an `INSERT ... ON DUPLICATE KEY UPDATE`
    /// assignment list — references the proposed (would-have-been-
    /// inserted) row's value for `col`. Valid only in that parse
    /// context; everywhere else the parser must reject it.
    ///
    /// `col_idx` is set by the analyzer to the column's position in the
    /// target table's schema; the parser emits `col_idx = 0`.
    InsertValue { col_idx: usize, name: String },

    /// `EXCLUDED.col` inside PostgreSQL `INSERT ... ON CONFLICT DO UPDATE`.
    ///
    /// References the proposed row's value for `col`, after INSERT defaults
    /// and coercions have been materialized. Like `InsertValue`, this is only
    /// valid in a conflict-resolution context and must be resolved by the
    /// analyzer before execution.
    ExcludedValue { col_idx: usize, name: String },

    /// SQL:2016 `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS` (Phase 11.19a).
    /// Special-form grammar with typed clause fields.
    SqlJsonQuery {
        kind: SqlJsonQueryKind,
        doc: Box<Expr>,
        path: String,
        path_mode: SqlJsonPathMode,
        /// PASSING bindings: (value expression, variable name). Phase 11.19c.
        passing: Vec<(Expr, String)>,
        returning: Option<axiomdb_types::DataType>,
        wrapper: SqlJsonWrapper,
        quotes: SqlJsonQuotes,
        on_empty: SqlJsonOnBehavior,
        on_error: SqlJsonOnBehavior,
    },

    /// A positional parameter placeholder — the `?` in a prepared statement.
    ///
    /// `idx` is the 0-based index of the parameter (first `?` → 0, second → 1).
    ///
    /// Produced by the parser when parsing a prepared statement SQL template.
    /// Must be replaced with `Expr::Literal(value)` via `substitute_params_in_ast`
    /// before the statement is executed. Reaching `eval_with` with an
    /// unsubstituted `Param` is a programming error.
    Param { idx: usize },

    /// `DEFAULT` — the column's declared default value.
    ///
    /// Valid in `INSERT ... VALUES (1, DEFAULT)` and `UPDATE t SET col = DEFAULT`.
    /// Resolved to `Value::Null` at eval time; the executor applies the column's
    /// declared default when default-expression persistence is implemented (4.18e).
    Default,

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

    // ── GROUPING SETS function (Phase 21.21) ──────────────────────────────────
    /// SQL standard `GROUPING(expr, ...)`.
    ///
    /// Returns a bitmask indicating which arguments are "rolled up" (NULLed by the
    /// current grouping set). Bit `n-1-i` corresponds to `args[i]` (leftmost = MSB,
    /// PostgreSQL-compatible).
    ///
    /// - `args`: raw argument expressions (set by the parser).
    /// - `universe_indices`: resolved by the analyzer from the current query's
    ///   `GroupByClause::Sets.universe`. `None` if outside a
    ///   GROUPING SETS context (evaluates to 0).
    ///
    /// At eval time, the last element of the current row is the hidden
    /// `__grouping_mask__` (a `Value::BigInt`). The evaluator reads it and
    /// computes the bitmask over `universe_indices`.
    Grouping {
        args: Vec<Expr>,
        /// Populated by the analyzer. `None` = pre-analysis or outside Sets context.
        universe_indices: Option<Vec<usize>>,
    },

    // ── ARRAY constructor (Phase 20.4) ──────────────────────────────────────
    /// `ARRAY[expr, ...]` — PostgreSQL-compatible array constructor.
    ///
    /// Parsed from `ARRAY[expr, ...]` syntax. The evaluator infers the common
    /// element type from all elements (all-same → that type; int+real → real).
    /// An empty `ARRAY[]` requires an explicit `::type[]` cast on the outside.
    ArrayConstructor { elements: Vec<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
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
    /// `XOR` — boolean exclusive-or.  NULL XOR x = NULL.
    Xor,

    // String
    /// `||` — string concatenation. Both operands must be `Text`.
    Concat,

    // Null-safe
    /// `<=>` — null-safe equality; returns Bool, never NULL.
    NullSafe,

    // Integer arithmetic
    /// `DIV` — integer division, truncates toward zero.  NULL propagates.
    IntDiv,

    // Bitwise
    /// `&` — bitwise AND.
    BitAnd,
    /// `|` — bitwise OR.
    BitOr,
    /// `^` — bitwise XOR.
    BitXor,
    /// `<<` — bitwise shift left.
    ShiftLeft,
    /// `>>` — bitwise shift right.
    ShiftRight,

    // Pattern matching
    /// `REGEXP` / `RLIKE` — regular-expression match.
    Regexp,

    // JSON operators (Phase 11.16 / 11.17)
    /// `->` — JSON sub-document extraction returning `Value::Jsonb`.
    /// `expr -> 'key'` returns the JSON object field as a binary JSONB value.
    /// `expr -> 0` returns the array element at that index as JSONB.
    JsonSub,
    /// `@>` — JSONB containment: `doc @> query` returns 1 if every key/value
    /// in `query` exists in `doc` (deep subset check). NULL if either operand
    /// is NULL. Evaluated by `jsonb_contains()`.
    JsonContains,
    /// `<@` — JSONB contained-by: `query <@ doc` ≡ `doc @> query`
    /// (Phase 11.18a, PG parity). Arguments are NOT swapped in the AST —
    /// the eval layer delegates to `jsonb_contains(rhs, lhs)` to preserve
    /// EXPLAIN fidelity.
    JsonContainedBy,
    /// `?` — JSONB key/array-string existence: `doc ? 'k'` returns true
    /// when `doc` is an object containing key `'k'`, or an array holding
    /// `'k'` as a string element (Phase 11.18a). Matches PG
    /// `jsonb_exists()` semantics.
    JsonExists,
    /// `@?` — JSONB JSONPath existence (Phase 11.21b): `doc @? 'jsonpath'`
    /// returns true when the path has at least one match. Equivalent to
    /// `jsonb_path_exists(doc, path)`.
    JsonbPathExists,
    /// `@@` — JSONB JSONPath match (Phase 11.21c): `doc @@ 'jsonpath'`
    /// treats the path result as a predicate and returns that bool, or NULL
    /// when the result is absent / multi-valued / non-boolean. Equivalent to
    /// `jsonb_path_match(doc, path)`.
    JsonbPathMatch,
    /// `?|` — JSONB any-key existence (Phase 11.18b): `doc ?| keys` returns
    /// true when at least one element of the RHS array matches a top-level
    /// key (object) or string element (array) in the LHS document. RHS is a
    /// JSONB array (no native `TEXT[]` — documented PG divergence).
    JsonExistsAny,
    /// `?&` — JSONB all-keys existence (Phase 11.18b): like `?|` but every
    /// RHS-array element must match.
    JsonExistsAll,
    /// `#>` — JSONB path-extract (Phase 11.18c): `doc #> '["a","b"]'` returns
    /// the sub-document at the path as JSONB. RHS is a JSONB array of path
    /// segments (string keys / numeric indices).
    JsonPathExtract,
    /// `#>>` — JSONB path-extract as TEXT (Phase 11.18c).
    JsonPathExtractText,
    /// `#-` — JSONB path-delete (Phase 11.18c): returns doc with the path
    /// pruned. RHS is a JSONB array of path segments.
    JsonPathDelete,
}

// ── SQL/JSON standard query functions (Phase 11.19a) ─────────────────────────

/// Which of the three SQL:2016 special-form functions this node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlJsonQueryKind {
    /// `JSON_VALUE` — scalar extraction with type coercion.
    Value,
    /// `JSON_QUERY` — subtree (object / array / scalar) extraction.
    Query,
    /// `JSON_EXISTS` — existence predicate.
    Exists,
}

/// Path-mode prefix inside the jsonpath string.
///
/// `strict` (SQL:2016 + PG default) raises on missing parts, type
/// mismatches, and index out-of-range; `lax` silently drops such parts
/// and auto-unwraps arrays for scalar extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlJsonPathMode {
    Strict,
    Lax,
}

/// ON EMPTY / ON ERROR handler.
///
/// `JSON_EXISTS` additionally accepts the `TrueLit` / `FalseLit` /
/// `Unknown` variants for `ON ERROR`; the other two functions use
/// `Null` / `Error` / `Default`.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlJsonOnBehavior {
    /// `NULL` — return SQL NULL (spec default when clause omitted).
    Null,
    /// `ERROR` — raise the condition as a runtime error.
    Error,
    /// `DEFAULT expr` — lazy-evaluate `expr` and return it, coerced to
    /// the `RETURNING` type when present.
    Default(Box<Expr>),
    /// `TRUE` — only valid on `JSON_EXISTS` `ON ERROR`.
    TrueLit,
    /// `FALSE` — only valid on `JSON_EXISTS` `ON ERROR`.
    FalseLit,
    /// `UNKNOWN` — only valid on `JSON_EXISTS` `ON ERROR`; maps to
    /// `Value::Null` in our 3VL model.
    Unknown,
}

/// `WITH [CONDITIONAL|UNCONDITIONAL] ARRAY WRAPPER` on JSON_QUERY (Phase 11.19b).
/// Default = Without (SQL:2016 + PG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlJsonWrapper {
    /// No wrapping; multi-item match routes via ON ERROR.
    Without,
    /// Always wrap in an array (even single items).
    Unconditional,
    /// Wrap only if the result is not already a single array (PG semantics).
    Conditional,
}

/// `KEEP|OMIT QUOTES [ON SCALAR STRING]` on JSON_QUERY (Phase 11.19b).
/// Default = Keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlJsonQuotes {
    Keep,
    Omit,
}

// ── UnaryOp ───────────────────────────────────────────────────────────────────

/// Unary operator for [`Expr::UnaryOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation: `-expr`.
    Neg,
    /// Boolean negation: `NOT expr`.
    Not,
    /// Bitwise NOT: `~expr`.
    BitNot,
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
