//! Session-aware text comparison for AxiomDB (Phase 5.2b).
//!
//! Provides two text-comparison behaviors:
//!
//! - [`SessionCollation::Binary`] — exact Rust string order (current default).
//! - [`SessionCollation::Es`] — NFC + lowercase + strip combining marks (CI+AI fold).
//!
//! The `Es` fold is intentionally **not** a full Spanish CLDR / ICU collation.
//! It is a lightweight session-level behavior that makes `Jose`, `JOSE`, and `José`
//! compare equal, matching the roadmap's `AXIOM_COMPAT = 'mysql'` goal.
//!
//! Full ICU / CLDR collation is deferred to Phase 13.13.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

use crate::session::SessionCollation;

// ── Canonical fold ────────────────────────────────────────────────────────────

/// Returns the canonical fold of `s` under the given collation.
///
/// - `Binary`: returns `s` unchanged (zero-alloc borrowed).
/// - `Es`: NFC → lowercase → NFD → strip combining marks → NFC.
///
/// The result is used for equality, hashing (GROUP BY / DISTINCT), and the
/// LIKE pattern match. For ordering, use [`compare_text`] which adds a raw
/// tie-break.
pub fn canonical_text<'a>(c: SessionCollation, s: &'a str) -> Cow<'a, str> {
    match c {
        SessionCollation::Binary => Cow::Borrowed(s),
        SessionCollation::Es => {
            // Step 1: NFC normalize.
            let nfc: String = s.nfc().collect();
            // Step 2: Unicode-aware lowercase.
            let lower = nfc.to_lowercase();
            // Step 3: NFD decompose + filter combining marks + re-NFC.
            let folded: String = lower
                .nfd()
                .filter(|ch| !unicode_normalization::char::is_combining_mark(*ch))
                .collect::<String>()
                .nfc()
                .collect();
            Cow::Owned(folded)
        }
    }
}

// ── Comparison ────────────────────────────────────────────────────────────────

/// Compares two text strings under the given collation.
///
/// - `Binary`: lexicographic byte order.
/// - `Es`: compare folded text first; if equal, break ties with the original
///   raw text for a deterministic total order.
///
/// The raw-text tie-break means that identical-looking but differently-encoded
/// strings (e.g. NFC `"café"` vs NFD `"cafe\u{301}"`) produce a stable order
/// even though they compare equal under `Es`.
pub fn compare_text(c: SessionCollation, a: &str, b: &str) -> std::cmp::Ordering {
    match c {
        SessionCollation::Binary => a.cmp(b),
        SessionCollation::Es => {
            let ca = canonical_text(c, a);
            let cb = canonical_text(c, b);
            let ord = ca.as_ref().cmp(cb.as_ref());
            if ord != std::cmp::Ordering::Equal {
                ord
            } else {
                // Tie-break: raw text for determinism.
                a.cmp(b)
            }
        }
    }
}

/// Returns `true` if `a` and `b` are equal under the given collation.
///
/// More efficient than `compare_text(...) == Equal` for `Es` because it
/// avoids the tie-break allocation when the folded strings differ.
pub fn text_eq(c: SessionCollation, a: &str, b: &str) -> bool {
    match c {
        SessionCollation::Binary => a == b,
        SessionCollation::Es => canonical_text(c, a) == canonical_text(c, b),
    }
}

// ── LIKE ─────────────────────────────────────────────────────────────────────

/// Evaluates `text LIKE pattern` under the given collation.
///
/// - `Binary`: delegates to the existing `like_match`.
/// - `Es`: folds both `text` and `pattern` before matching.
///
/// LIKE with `Es` collation means `LIKE 'jos%'` matches `José`, `JOSE`, and
/// `jose` because all three fold to `jose` before the wildcard match runs.
pub fn like_match_collated(c: SessionCollation, text: &str, pattern: &str) -> bool {
    match c {
        SessionCollation::Binary => crate::eval::like_match(text, pattern),
        SessionCollation::Es => {
            let t = canonical_text(c, text);
            let p = canonical_text(c, pattern);
            crate::eval::like_match(&t, &p)
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use SessionCollation::{Binary, Es};

    #[test]
    fn binary_unchanged() {
        assert_eq!(
            canonical_text(Binary, "José"),
            std::borrow::Cow::Borrowed("José")
        );
        assert_eq!(compare_text(Binary, "a", "b"), std::cmp::Ordering::Less);
        assert!(text_eq(Binary, "jose", "jose"));
        assert!(!text_eq(Binary, "jose", "José"));
    }

    #[test]
    fn es_strips_accents_and_case() {
        assert!(text_eq(Es, "jose", "José"));
        assert!(text_eq(Es, "JOSE", "josé"));
        assert!(text_eq(Es, "Jose", "JOSE"));
    }

    #[test]
    fn es_compare_equal_folded() {
        // text_eq compares folded text only — jose and José are equal.
        assert!(
            text_eq(Es, "jose", "José"),
            "jose and José must be Es-equal"
        );
        // compare_text returns Equal when both the folded AND raw text match.
        assert_eq!(compare_text(Es, "jose", "jose"), std::cmp::Ordering::Equal);
        // When folds match but raw differs, compare_text applies a tie-break → non-Equal.
        assert_ne!(
            compare_text(Es, "jose", "José"),
            std::cmp::Ordering::Equal,
            "tie-break must produce a definite order when raw strings differ"
        );
    }

    #[test]
    fn es_compare_different_folded() {
        // "ana" vs "bob" — different folded values.
        assert_eq!(compare_text(Es, "ana", "bob"), std::cmp::Ordering::Less);
    }

    #[test]
    fn es_deterministic_tie_break() {
        // Two strings that are Es-equal but differ in raw form.
        // Tie-break should produce a deterministic non-Equal result.
        let ord = compare_text(Es, "jose", "Jose");
        // jose < Jose in raw ordering (lowercase before uppercase in UTF-8)
        // Actually in Rust, 'j' (0x6A) < 'J' (0x4A)? No: 'J' = 0x4A, 'j' = 0x6A.
        // So "Jose" < "jose" in raw order → compare_text should return Greater for ("jose", "Jose")
        assert_ne!(
            ord,
            std::cmp::Ordering::Equal,
            "tie-break must produce a definite order"
        );
    }

    #[test]
    fn like_binary_case_sensitive() {
        assert!(like_match_collated(Binary, "José", "José"));
        assert!(!like_match_collated(Binary, "José", "jose"));
        assert!(!like_match_collated(Binary, "José", "jos%"));
    }

    #[test]
    fn like_es_case_insensitive_accent_insensitive() {
        assert!(like_match_collated(Es, "José", "jos%"));
        assert!(like_match_collated(Es, "JOSE", "jose"));
        assert!(like_match_collated(Es, "José", "JOS_"));
    }
}
