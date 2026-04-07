// ── Public types ──────────────────────────────────────────────────────────────

/// Controls how [`coerce`] handles ambiguous conversions.
///
/// Use [`CoercionMode::Strict`] (the AxiomDB default) for correct behavior.
/// Use [`CoercionMode::Permissive`] when the session is in MySQL-compat mode
/// (`SET AXIOM_COMPAT = 'mysql'` — Phase 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoercionMode {
    /// AxiomDB default.
    ///
    /// `Text → numeric` requires the entire string (after trimming whitespace)
    /// to be a valid number. `'42abc'` → `INT` is an error. `'42'` → `INT` is
    /// `Int(42)`. `Bool → numeric` is always an error.
    Strict,

    /// MySQL-compatible lenient mode.
    ///
    /// `Text → numeric` parses as many leading numeric characters as possible
    /// and discards the rest. `'42abc'` → `Int(42)`. `'abc'` → `Int(0)`.
    /// `Bool → Int/BigInt/Real` succeeds: `true` → `1`, `false` → `0`.
    Permissive,
}
