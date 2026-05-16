//! Phase 20.20 — XML scalar function evaluation.
//!
//! Functions here are "pure" (no storage access). They are dispatched from
//! `eval_function` in `mod.rs` by name.
//!
//! Step 1 functions: `xml_is_well_formed`
//! Step 2 functions: XMLELEMENT, XMLFOREST, XMLROOT, XMLCONCAT, XMLQUERY
//!   (those are Expr variants evaluated in expr.rs, not name-dispatched here)

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

// ── xml_is_well_formed ────────────────────────────────────────────────────────

pub(super) fn eval_is_well_formed(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "xml_is_well_formed(text)".into(),
            got: format!("{} arguments", args.len()),
        });
    }
    let v = crate::eval::eval(&args[0], row)?;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Text(s) | Value::Xml(s) => {
            let ok = axiomdb_types::validate_xml_text(&s).is_ok();
            Ok(Value::Int(if ok { 1 } else { 0 }))
        }
        other => Err(DbError::TypeMismatch {
            expected: "TEXT or XML".into(),
            got: other.variant_name().to_string(),
        }),
    }
}
