//! FTS query parser and matcher (Phase 11.7).
//!
//! Parses boolean FTS queries into a tree of requirements:
//! - `+term` — required (AND)
//! - `-term` — excluded (NOT)
//! - `term` — optional (boosts score if present)
//! - `term1 | term2` — OR (either matches)
//! - `"exact phrase"` — consecutive tokens at adjacent positions
//! - `prefix*` — prefix match (any term starting with prefix)
//!
//! ## PostgreSQL reference
//! PostgreSQL tsquery uses `&` (AND), `|` (OR), `!` (NOT), `<->` (phrase).
//! AxiomDB uses MySQL FULLTEXT syntax: `+`, `-`, `|`, `"..."`, `*`.

use crate::tokenizer::{tokenize, Token};

/// A parsed FTS query clause.
#[derive(Debug, Clone)]
pub enum FtsClause {
    /// Term must be present.
    Required(String),
    /// Term must NOT be present.
    Excluded(String),
    /// Term is optional (boosts score).
    Optional(String),
    /// Either side matches (OR).
    Or(Box<FtsClause>, Box<FtsClause>),
    /// Exact phrase — consecutive tokens.
    Phrase(Vec<String>),
    /// Prefix match — any term starting with this prefix.
    Prefix(String),
}

/// Parses an FTS query string into clauses.
///
/// Syntax: `+required -excluded optional "exact phrase" prefix*`
pub fn parse_fts_query(query: &str) -> Vec<FtsClause> {
    let mut clauses = Vec::new();
    let mut chars = query.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' => {
                chars.next();
            }
            '"' => {
                chars.next(); // consume opening quote
                let mut phrase = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch == '"' {
                        chars.next();
                        break;
                    }
                    phrase.push(ch);
                    chars.next();
                }
                let terms: Vec<String> = phrase
                    .split_whitespace()
                    .map(|w| w.to_lowercase())
                    .filter(|w| !w.is_empty())
                    .collect();
                if !terms.is_empty() {
                    clauses.push(FtsClause::Phrase(terms));
                }
            }
            '+' => {
                chars.next();
                let term = read_term(&mut chars);
                if !term.is_empty() {
                    if term.ends_with('*') {
                        clauses.push(FtsClause::Required(term.trim_end_matches('*').to_string()));
                    } else {
                        clauses.push(FtsClause::Required(term));
                    }
                }
            }
            '-' => {
                chars.next();
                let term = read_term(&mut chars);
                if !term.is_empty() {
                    clauses.push(FtsClause::Excluded(term));
                }
            }
            _ => {
                let term = read_term(&mut chars);
                if term.is_empty() {
                    continue;
                }
                // Check for OR: "term1 | term2"
                skip_spaces(&mut chars);
                if chars.peek() == Some(&'|') {
                    chars.next(); // consume |
                    skip_spaces(&mut chars);
                    let term2 = read_term(&mut chars);
                    if !term2.is_empty() {
                        clauses.push(FtsClause::Or(
                            Box::new(FtsClause::Optional(term.clone())),
                            Box::new(FtsClause::Optional(term2)),
                        ));
                        continue;
                    }
                }
                if term.ends_with('*') {
                    clauses.push(FtsClause::Prefix(term.trim_end_matches('*').to_string()));
                } else {
                    clauses.push(FtsClause::Optional(term));
                }
            }
        }
    }
    clauses
}

fn read_term(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut term = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '\'' || c == '*' || c == '_' {
            term.push(c);
            chars.next();
        } else {
            break;
        }
    }
    term.to_lowercase()
}

fn skip_spaces(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c == ' ' || c == '\t' {
            chars.next();
        } else {
            break;
        }
    }
}

/// Evaluates an FTS query against a document. Returns a relevance score (0.0 = no match).
///
/// Scoring:
/// - Required (+term): must be present, contributes 1.0 per match
/// - Excluded (-term): if present, entire doc scores 0.0
/// - Optional (term): contributes 0.5 per match
/// - Or (a | b): contributes 0.75 if either matches
/// - Phrase ("a b"): contributes 2.0 if found at adjacent positions
/// - Prefix (term*): contributes 0.5 per matching term
pub fn evaluate_fts(clauses: &[FtsClause], text: &str) -> f64 {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return 0.0;
    }

    let term_set: std::collections::HashSet<&str> =
        tokens.iter().map(|t| t.term.as_str()).collect();

    let mut score = 0.0;
    let mut required_met = true;

    for clause in clauses {
        match clause {
            FtsClause::Required(term) => {
                if term_set.contains(term.as_str()) {
                    score += 1.0;
                } else {
                    required_met = false;
                }
            }
            FtsClause::Excluded(term) => {
                if term_set.contains(term.as_str()) {
                    return 0.0; // excluded term present — no match
                }
            }
            FtsClause::Optional(term) => {
                if term_set.contains(term.as_str()) {
                    score += 0.5;
                }
            }
            FtsClause::Or(a, b) => {
                let a_match = clause_matches(a, &term_set, &tokens);
                let b_match = clause_matches(b, &term_set, &tokens);
                if a_match || b_match {
                    score += 0.75;
                }
            }
            FtsClause::Phrase(terms) => {
                if phrase_matches(terms, &tokens) {
                    score += 2.0;
                } else {
                    // Phrase is implicitly required.
                    required_met = false;
                }
            }
            FtsClause::Prefix(prefix) => {
                if tokens.iter().any(|t| t.term.starts_with(prefix.as_str())) {
                    score += 0.5;
                }
            }
        }
    }

    if !required_met {
        return 0.0;
    }
    score
}

fn clause_matches(
    clause: &FtsClause,
    term_set: &std::collections::HashSet<&str>,
    tokens: &[Token],
) -> bool {
    match clause {
        FtsClause::Required(t) | FtsClause::Optional(t) => term_set.contains(t.as_str()),
        FtsClause::Prefix(p) => tokens.iter().any(|t| t.term.starts_with(p.as_str())),
        FtsClause::Phrase(terms) => phrase_matches(terms, tokens),
        _ => false,
    }
}

/// Checks if a phrase (consecutive terms) appears at adjacent positions.
fn phrase_matches(phrase_terms: &[String], tokens: &[Token]) -> bool {
    if phrase_terms.is_empty() {
        return true;
    }
    if phrase_terms.len() > tokens.len() {
        return false;
    }
    // Find first term, then check if subsequent terms are at consecutive positions.
    for (i, tok) in tokens.iter().enumerate() {
        if tok.term == phrase_terms[0] {
            let mut all_match = true;
            for (j, pt) in phrase_terms.iter().enumerate().skip(1) {
                if let Some(next) = tokens.get(i + j) {
                    if next.term != *pt || next.position != tok.position + j as u32 {
                        all_match = false;
                        break;
                    }
                } else {
                    all_match = false;
                    break;
                }
            }
            if all_match {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_query() {
        let clauses = parse_fts_query("rust database");
        assert_eq!(clauses.len(), 2);
        assert!(matches!(&clauses[0], FtsClause::Optional(t) if t == "rust"));
    }

    #[test]
    fn test_required_excluded() {
        let clauses = parse_fts_query("+rust -python");
        assert!(matches!(&clauses[0], FtsClause::Required(t) if t == "rust"));
        assert!(matches!(&clauses[1], FtsClause::Excluded(t) if t == "python"));
    }

    #[test]
    fn test_phrase() {
        let clauses = parse_fts_query("\"database engine\"");
        assert!(matches!(&clauses[0], FtsClause::Phrase(terms) if terms.len() == 2));
    }

    #[test]
    fn test_prefix() {
        let clauses = parse_fts_query("data*");
        assert!(matches!(&clauses[0], FtsClause::Prefix(p) if p == "data"));
    }

    #[test]
    fn test_or() {
        let clauses = parse_fts_query("rust | golang");
        assert!(matches!(&clauses[0], FtsClause::Or(_, _)));
    }

    #[test]
    fn test_evaluate_required() {
        let clauses = parse_fts_query("+rust +database");
        assert!(evaluate_fts(&clauses, "Rust is a database engine") > 0.0);
        assert_eq!(evaluate_fts(&clauses, "Python is great"), 0.0);
    }

    #[test]
    fn test_evaluate_excluded() {
        let clauses = parse_fts_query("+rust -python");
        assert!(evaluate_fts(&clauses, "Rust database") > 0.0);
        assert_eq!(evaluate_fts(&clauses, "Rust and Python"), 0.0);
    }

    #[test]
    fn test_evaluate_phrase() {
        let clauses = parse_fts_query("\"database engine\"");
        assert!(evaluate_fts(&clauses, "Rust is a fast database engine") > 0.0);
        assert_eq!(evaluate_fts(&clauses, "database and engine"), 0.0); // not adjacent
    }

    #[test]
    fn test_evaluate_prefix() {
        let clauses = parse_fts_query("data*");
        assert!(evaluate_fts(&clauses, "This database is good") > 0.0);
        assert_eq!(evaluate_fts(&clauses, "Nothing matches here"), 0.0);
    }
}
