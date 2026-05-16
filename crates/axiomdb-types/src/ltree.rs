//! ltree — hierarchical label-path type (Phase 20.19).
//!
//! Validation and pure path operations. No I/O or catalog access.

use axiomdb_core::DbError;

// ── Validation ────────────────────────────────────────────────────────────────

/// Validate that `s` is a well-formed ltree path.
///
/// Rules:
/// - At least one label.
/// - Labels separated by `.`; no leading, trailing, or consecutive dots.
/// - Each label: `[A-Za-z0-9_]+`, max 255 bytes.
/// - Total path: max 65 535 bytes.
pub fn validate_ltree_path(s: &str) -> Result<(), DbError> {
    if s.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "ltree path cannot be empty".into(),
        });
    }
    if s.len() > 65535 {
        return Err(DbError::InvalidValue {
            reason: "ltree path too long (max 65535 bytes)".into(),
        });
    }
    for label in s.split('.') {
        if label.is_empty() {
            return Err(DbError::InvalidValue {
                reason: "ltree path contains empty label (leading/trailing/consecutive dots)"
                    .into(),
            });
        }
        if label.len() > 255 {
            return Err(DbError::InvalidValue {
                reason: format!("ltree label '{label}' exceeds 255 bytes"),
            });
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "ltree label '{label}' contains invalid characters (only [A-Za-z0-9_] allowed)"
                ),
            });
        }
    }
    Ok(())
}

// ── Ancestor / descendant ─────────────────────────────────────────────────────

/// Return true if `ancestor` is an ancestor-or-equal of `path`.
///
/// `ancestor` is an ancestor of `path` iff `path` starts with `ancestor`
/// followed immediately by `.` (label boundary), or they are equal.
pub fn ltree_is_ancestor(ancestor: &str, path: &str) -> bool {
    if ancestor == path {
        return true;
    }
    path.starts_with(ancestor) && path.as_bytes().get(ancestor.len()) == Some(&b'.')
}

// ── Concatenation ─────────────────────────────────────────────────────────────

/// Concatenate two ltree paths: `left.right`.
pub fn ltree_concat(left: &str, right: &str) -> String {
    format!("{left}.{right}")
}

// ── lquery pattern matching ───────────────────────────────────────────────────

/// Match `path` against an lquery pattern.
///
/// Pattern labels:
/// - Exact label string → must match the corresponding path label exactly.
/// - `*` → matches zero or more consecutive labels.
///
/// The entire path must be consumed when the entire pattern is consumed
/// (anchored match).
pub fn lquery_match(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('.').collect();
    let pat_parts: Vec<&str> = pattern.split('.').collect();
    lquery_match_parts(&path_parts, &pat_parts)
}

fn lquery_match_parts(path: &[&str], pat: &[&str]) -> bool {
    match (path, pat) {
        ([], []) => true,
        // Pattern exhausted but path remaining → no match
        (_, []) => false,
        // Path exhausted; only `*` tokens can still match
        ([], [p, rest @ ..]) => *p == "*" && lquery_match_parts(&[], rest),
        ([_ph, pr @ ..], [p, pr2 @ ..]) if *p == "*" => {
            // `*` matches 0 labels: skip the `*` in the pattern
            lquery_match_parts(path, pr2)
            // OR `*` matches 1+ labels: consume one path label, keep `*`
                || lquery_match_parts(pr, pat)
        }
        ([ph, pr @ ..], [p, pr2 @ ..]) => *ph == *p && lquery_match_parts(pr, pr2),
    }
}

// ── Scalar functions ──────────────────────────────────────────────────────────

/// Return the number of labels in `path`.
pub fn ltree_nlevel(path: &str) -> usize {
    path.split('.').count()
}

/// Return a sub-path starting at `offset` (0-based) with optional `len`.
///
/// Returns `None` if `offset >= nlevel(path)`.
/// If `offset + len > nlevel(path)`, clips to the end (no error).
pub fn ltree_subpath(path: &str, offset: usize, len: Option<usize>) -> Option<String> {
    let labels: Vec<&str> = path.split('.').collect();
    if offset >= labels.len() {
        return None;
    }
    let end = match len {
        Some(l) => (offset + l).min(labels.len()),
        None => labels.len(),
    };
    Some(labels[offset..end].join("."))
}

/// Return position (0-based) of `subpath` as consecutive labels within `path`,
/// starting search at `offset`. Returns `None` if not found.
pub fn ltree_index(path: &str, subpath: &str, offset: usize) -> Option<usize> {
    let path_labels: Vec<&str> = path.split('.').collect();
    let sub_labels: Vec<&str> = subpath.split('.').collect();
    let n = path_labels.len();
    let m = sub_labels.len();
    if m == 0 || m > n {
        return None;
    }
    let limit = n.saturating_sub(m);
    if offset > limit {
        return None;
    }
    for i in offset..=limit {
        if path_labels[i..i + m] == sub_labels[..] {
            return Some(i);
        }
    }
    None
}

/// Return the longest common ancestor of all `paths`.
///
/// Returns an empty string if there is no common prefix (disjoint paths).
/// Returns the path itself if all paths are identical.
pub fn ltree_lca(paths: &[&str]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let first_labels: Vec<&str> = paths[0].split('.').collect();
    let mut common_len = first_labels.len();
    for path in &paths[1..] {
        let labels: Vec<&str> = path.split('.').collect();
        let cmp_len = common_len.min(labels.len());
        let mut matched = 0;
        for (a, b) in first_labels[..cmp_len].iter().zip(labels[..cmp_len].iter()) {
            if a == b {
                matched += 1;
            } else {
                break;
            }
        }
        common_len = matched;
    }
    first_labels[..common_len].join(".")
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_ltree_path ──────────────────────────────────────────────────

    #[test]
    fn valid_single_label() {
        assert!(validate_ltree_path("a").is_ok());
    }
    #[test]
    fn valid_multi_label() {
        assert!(validate_ltree_path("a.b.c").is_ok());
    }
    #[test]
    fn valid_underscore() {
        assert!(validate_ltree_path("foo_bar.baz_2").is_ok());
    }
    #[test]
    fn reject_empty() {
        assert!(validate_ltree_path("").is_err());
    }
    #[test]
    fn reject_leading_dot() {
        assert!(validate_ltree_path(".a").is_err());
    }
    #[test]
    fn reject_trailing_dot() {
        assert!(validate_ltree_path("a.").is_err());
    }
    #[test]
    fn reject_consecutive_dots() {
        assert!(validate_ltree_path("a..b").is_err());
    }
    #[test]
    fn reject_space() {
        assert!(validate_ltree_path("a b").is_err());
    }
    #[test]
    fn reject_bang() {
        assert!(validate_ltree_path("a!b").is_err());
    }
    #[test]
    fn reject_hyphen() {
        assert!(validate_ltree_path("a-b").is_err());
    }

    // ── ltree_is_ancestor ────────────────────────────────────────────────────

    #[test]
    fn ancestor_equal_paths() {
        assert!(ltree_is_ancestor("a.b", "a.b"));
    }
    #[test]
    fn ancestor_prefix() {
        assert!(ltree_is_ancestor("a.b", "a.b.c"));
    }
    #[test]
    fn ancestor_single_label() {
        assert!(ltree_is_ancestor("a", "a.b.c"));
    }
    #[test]
    fn not_ancestor_shorter() {
        assert!(!ltree_is_ancestor("a.b.c", "a.b"));
    }
    #[test]
    fn not_ancestor_partial_label() {
        // 'a.b' is NOT ancestor of 'a.bc' (different label)
        assert!(!ltree_is_ancestor("a.b", "a.bc"));
    }
    #[test]
    fn not_ancestor_disjoint() {
        assert!(!ltree_is_ancestor("x.y", "a.b.c"));
    }

    // ── lquery_match ─────────────────────────────────────────────────────────

    #[test]
    fn lquery_star_matches_any() {
        assert!(lquery_match("a.b.c", "*"));
    }
    #[test]
    fn lquery_star_prefix() {
        assert!(lquery_match("a.b.c", "a.*"));
    }
    #[test]
    fn lquery_mid_star() {
        assert!(lquery_match("a.b.c", "*.b.*"));
    }
    #[test]
    fn lquery_suffix_star() {
        assert!(lquery_match("a.b.c", "*.c"));
    }
    #[test]
    fn lquery_exact_match() {
        assert!(lquery_match("a.b", "a.b"));
    }
    #[test]
    fn lquery_exact_no_match() {
        // pattern doesn't consume the full path
        assert!(!lquery_match("a.b.c", "a.b"));
    }
    #[test]
    fn lquery_star_matches_zero() {
        // '*.c' must match 'c' (star matches 0 labels)
        assert!(lquery_match("c", "*.c"));
    }
    #[test]
    fn lquery_star_only_single_label() {
        assert!(lquery_match("a", "*"));
    }

    // ── ltree_nlevel ─────────────────────────────────────────────────────────

    #[test]
    fn nlevel_single() {
        assert_eq!(ltree_nlevel("a"), 1);
    }
    #[test]
    fn nlevel_three() {
        assert_eq!(ltree_nlevel("a.b.c"), 3);
    }

    // ── ltree_subpath ─────────────────────────────────────────────────────────

    #[test]
    fn subpath_from_offset() {
        assert_eq!(ltree_subpath("a.b.c.d", 1, None), Some("b.c.d".into()));
    }
    #[test]
    fn subpath_with_len() {
        assert_eq!(ltree_subpath("a.b.c.d", 1, Some(2)), Some("b.c".into()));
    }
    #[test]
    fn subpath_full() {
        assert_eq!(ltree_subpath("a.b.c", 0, None), Some("a.b.c".into()));
    }
    #[test]
    fn subpath_single_last_label() {
        assert_eq!(ltree_subpath("a.b.c", 2, Some(1)), Some("c".into()));
    }
    #[test]
    fn subpath_len_clips_to_end() {
        assert_eq!(ltree_subpath("a.b", 0, Some(100)), Some("a.b".into()));
    }
    #[test]
    fn subpath_offset_out_of_range() {
        assert!(ltree_subpath("a.b", 5, None).is_none());
    }

    // ── ltree_index ───────────────────────────────────────────────────────────

    #[test]
    fn index_found_at_zero() {
        assert_eq!(ltree_index("a.b.c.a.b", "a.b", 0), Some(0));
    }
    #[test]
    fn index_found_with_offset() {
        assert_eq!(ltree_index("a.b.c.a.b", "a.b", 1), Some(3));
    }
    #[test]
    fn index_not_found() {
        assert_eq!(ltree_index("a.b.c", "x.y", 0), None);
    }
    #[test]
    fn index_exact_path() {
        assert_eq!(ltree_index("a.b", "a.b", 0), Some(0));
    }

    // ── ltree_lca ────────────────────────────────────────────────────────────

    #[test]
    fn lca_common_ancestor() {
        assert_eq!(ltree_lca(&["a.b.c", "a.b.d"]), "a.b");
    }
    #[test]
    fn lca_disjoint() {
        assert_eq!(ltree_lca(&["a.b", "c.d"]), "");
    }
    #[test]
    fn lca_identical() {
        assert_eq!(ltree_lca(&["a.b", "a.b"]), "a.b");
    }
    #[test]
    fn lca_single_input() {
        assert_eq!(ltree_lca(&["a.b.c"]), "a.b.c");
    }
    #[test]
    fn lca_empty_slice() {
        assert_eq!(ltree_lca(&[]), "");
    }
    #[test]
    fn lca_three_paths() {
        assert_eq!(ltree_lca(&["a.b.c.d", "a.b.c.e", "a.b.x"]), "a.b");
    }
}
