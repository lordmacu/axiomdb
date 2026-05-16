//! Phase 20.20 — XML scalar function evaluation.
//!
//! Functions here are "pure" (no storage access). They are dispatched from
//! `eval_function` in `mod.rs` by name, or directly from `eval/core.rs` for
//! the special-form Expr variants (XmlElement, XmlForest, XmlRoot, XmlConcat,
//! XmlQuery).

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::eval::SubqueryRunner;
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

// ── XMLELEMENT ────────────────────────────────────────────────────────────────

pub(crate) fn eval_xmlelement<R: SubqueryRunner>(
    tag: &str,
    attrs: &[(Expr, String)],
    content: &[Expr],
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    validate_xml_name(tag)?;

    let mut result = format!("<{tag}");

    for (v_expr, a_name) in attrs {
        validate_xml_name(a_name)?;
        let v = crate::eval::eval_with(v_expr, row, sq)?;
        if matches!(v, Value::Null) {
            continue;
        }
        let text = value_to_text(&v);
        result.push(' ');
        result.push_str(a_name);
        result.push_str("=\"");
        result.push_str(&escape_attr(&text));
        result.push('"');
    }

    result.push('>');

    for c_expr in content {
        let v = crate::eval::eval_with(c_expr, row, sq)?;
        match v {
            Value::Null => {}
            Value::Xml(s) => result.push_str(&s),
            other => result.push_str(&escape_text(&value_to_text(&other))),
        }
    }

    result.push_str("</");
    result.push_str(tag);
    result.push('>');

    Ok(Value::Xml(result))
}

// ── XMLFOREST ─────────────────────────────────────────────────────────────────

pub(crate) fn eval_xmlforest<R: SubqueryRunner>(
    items: &[(Expr, String)],
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let mut result = String::new();
    for (expr, name) in items {
        validate_xml_name(name)?;
        let v = crate::eval::eval_with(expr, row, sq)?;
        if matches!(v, Value::Null) {
            continue;
        }
        let text = escape_text(&value_to_text(&v));
        result.push('<');
        result.push_str(name);
        result.push('>');
        result.push_str(&text);
        result.push_str("</");
        result.push_str(name);
        result.push('>');
    }
    Ok(Value::Xml(result))
}

// ── XMLROOT ───────────────────────────────────────────────────────────────────

pub(crate) fn eval_xmlroot<R: SubqueryRunner>(
    doc_expr: &Expr,
    version: &str,
    standalone: Option<bool>,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let v = crate::eval::eval_with(doc_expr, row, sq)?;
    let xml_str = match v {
        Value::Null => return Ok(Value::Null),
        Value::Xml(s) | Value::Text(s) => s,
        other => value_to_text(&other),
    };

    let body = strip_xml_decl(&xml_str);

    let standalone_attr = match standalone {
        Some(true) => " standalone=\"yes\"",
        Some(false) => " standalone=\"no\"",
        None => "",
    };

    let decl = format!("<?xml version=\"{version}\"{standalone_attr}?>");
    Ok(Value::Xml(format!("{decl}{body}")))
}

// ── XMLCONCAT ─────────────────────────────────────────────────────────────────

pub(crate) fn eval_xmlconcat<R: SubqueryRunner>(
    args: &[Expr],
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let mut result = String::new();
    let mut all_null = true;
    for arg in args {
        let v = crate::eval::eval_with(arg, row, sq)?;
        match v {
            Value::Null => {}
            Value::Xml(s) | Value::Text(s) => {
                all_null = false;
                result.push_str(&s);
            }
            other => {
                all_null = false;
                result.push_str(&value_to_text(&other));
            }
        }
    }
    if all_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Xml(result))
    }
}

// ── XMLQUERY ──────────────────────────────────────────────────────────────────

pub(crate) fn eval_xmlquery<R: SubqueryRunner>(
    xpath: &str,
    doc_expr: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let v = crate::eval::eval_with(doc_expr, row, sq)?;
    let xml_str = match v {
        Value::Null => return Ok(Value::Null),
        Value::Xml(s) | Value::Text(s) => s,
        other => value_to_text(&other),
    };

    let doc = match roxmltree::Document::parse(&xml_str) {
        Ok(d) => d,
        Err(_) => return Ok(Value::Null),
    };

    match eval_xpath(&doc, xpath) {
        Some(s) => Ok(Value::Text(s)),
        None => Ok(Value::Null),
    }
}

// ── Minimal XPath evaluator (for XMLQUERY) ───────────────────────────────────
//
// Supports a subset of XPath 1.0 (absolute paths only):
//   /elem/elem/.../text()      — navigate elements, extract text content
//   /elem/elem/.../@attr       — navigate elements, extract attribute value
//   /elem/elem/...             — navigate to element, return its text content
// All paths must start with `/`.
// Element steps match on local name only (no namespace prefix).

fn eval_xpath(doc: &roxmltree::Document<'_>, xpath: &str) -> Option<String> {
    let xpath = xpath.trim();
    if !xpath.starts_with('/') {
        return None;
    }

    let path = &xpath[1..]; // strip leading /

    // Split into steps; last step may be text() or @attr
    let steps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if steps.is_empty() {
        return None;
    }

    // Determine the final step kind
    let (nav_steps, final_step) = match steps.last() {
        Some(&"text()") => (&steps[..steps.len() - 1], XPathFinalStep::Text),
        Some(last) if last.starts_with('@') => {
            (&steps[..steps.len() - 1], XPathFinalStep::Attr(&last[1..]))
        }
        _ => (&steps[..], XPathFinalStep::Element),
    };

    let root = doc.root_element();

    // For absolute paths: the first step matches against the root element.
    // Subsequent steps navigate into its children.
    let mut current: Vec<roxmltree::Node<'_, '_>> = if nav_steps.is_empty() {
        vec![root]
    } else {
        // First step: root element must match
        if root.tag_name().name() != nav_steps[0] {
            return None;
        }
        // Subsequent steps: find matching children of each matched node
        let mut nodes = vec![root];
        for step in &nav_steps[1..] {
            let mut next = Vec::new();
            for node in &nodes {
                for child in node.children() {
                    if child.is_element() && child.tag_name().name() == *step {
                        next.push(child);
                    }
                }
            }
            if next.is_empty() {
                return None;
            }
            nodes = next;
        }
        nodes
    };

    let node = current.drain(..).next()?;

    match final_step {
        XPathFinalStep::Text => {
            let text: String = node
                .children()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        XPathFinalStep::Attr(name) => node.attribute(name).map(|s| s.to_string()),
        XPathFinalStep::Element => {
            let text: String = node
                .children()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
    }
}

enum XPathFinalStep<'a> {
    Text,
    Attr(&'a str),
    Element,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

fn strip_xml_decl(s: &str) -> &str {
    let s = s.trim_start();
    if s.starts_with("<?xml") {
        if let Some(end) = s.find("?>") {
            return &s[end + 2..];
        }
    }
    s
}

fn validate_xml_name(s: &str) -> Result<(), DbError> {
    if s.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "XML element/attribute name must not be empty".into(),
        });
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(DbError::InvalidValue {
            reason: format!("XML name '{s}' has invalid first character '{first}'"),
        });
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '.') {
            return Err(DbError::InvalidValue {
                reason: format!("XML name '{s}' has invalid character '{c}'"),
            });
        }
    }
    Ok(())
}

fn value_to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Text(s) | Value::Xml(s) | Value::Json(s) => s.clone(),
        other => format!("{other}"),
    }
}
