//! Phase 20.20 — XML type primitives.
//!
//! Provides well-formedness validation for `Value::Xml` using `roxmltree`.
//! All SQL-level XML operations (XMLELEMENT, XMLTABLE, etc.) live in
//! `axiomdb-sql`. This module only handles the `axiomdb-types` contract:
//! validation on coerce, and the public `validate_xml_text` function.

/// Validate that `s` is a well-formed XML document or fragment.
///
/// Accepts both full XML documents (with `<?xml?>` declaration or a single
/// root element) and fragments (multiple sibling elements).
///
/// Returns `Ok(())` if valid, `Err(reason_string)` describing the parse error.
pub fn validate_xml_text(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty string is not valid XML".into());
    }
    // roxmltree requires a single root element; wrap in a synthetic root to
    // accept fragments, then fall back to direct parse for full documents.
    match roxmltree::Document::parse(s) {
        Ok(_) => Ok(()),
        Err(first_err) => {
            // Try fragment: wrap in <_></_> and re-parse.
            let wrapped = format!("<_>{s}</_>");
            match roxmltree::Document::parse(&wrapped) {
                Ok(_) => Ok(()),
                Err(_) => Err(first_err.to_string()),
            }
        }
    }
}
