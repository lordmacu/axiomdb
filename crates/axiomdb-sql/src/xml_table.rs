//! Phase 20.20 — XMLTABLE table-valued function.
//!
//! Mirrors `json_table.rs` in architecture:
//! - `compile_xml_table` converts the AST into a `XmlTableSpec`
//! - `materialize_xml_table` shreds an XML document value into rows
//!
//! XPath support: absolute paths (`/elem/...`) with `text()`, `@attr`,
//! wildcard `*`, and positional `[n]` predicates.

use axiomdb_core::error::DbError;
use axiomdb_types::{coerce, CoercionMode, DataType, Value};

use crate::ast::XmlTable;
use crate::result::ColumnMeta;

// ── Compiled spec ─────────────────────────────────────────────────────────────

pub struct XmlTableSpec {
    pub alias: String,
    pub row_path: Vec<XPathStep>,
    pub columns: Vec<XmlTableColumnSpec>,
}

pub struct XmlTableColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub path: Vec<XPathStep>,
    pub default_expr: Option<crate::expr::Expr>,
    pub not_null: bool,
    pub ordinality: bool,
}

// ── XPath step types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum XPathStep {
    /// Named child element: `foo`
    Child(String),
    /// Any child element: `*`
    Wildcard,
    /// Descendant axis: `//foo`
    Descendant(String),
    /// Attribute: `@attr`
    Attr(String),
    /// Text node: `text()`
    Text,
    /// Positional predicate: `[n]` (1-based)
    Position(usize),
}

// ── XPath parser ──────────────────────────────────────────────────────────────

pub fn parse_xpath(xpath: &str) -> Result<Vec<XPathStep>, DbError> {
    let xpath = xpath.trim();
    if xpath.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "XPath expression must not be empty".into(),
        });
    }

    let mut steps: Vec<XPathStep> = Vec::new();
    let mut rest = xpath;

    // Absolute path: strip leading /
    if rest.starts_with('/') {
        rest = &rest[1..];
    }

    // Split on '/' but handle '//' (descendant axis) and trailing predicates
    while !rest.is_empty() {
        // '//' → descendant axis
        if rest.starts_with('/') {
            rest = &rest[1..]; // consume second /
            let (name, remaining) = consume_name_and_predicate(rest);
            let name = name.to_string();
            let (remaining, pos) = consume_position_predicate(remaining)?;
            rest = remaining;
            steps.push(XPathStep::Descendant(name));
            if let Some(p) = pos {
                steps.push(XPathStep::Position(p));
            }
            continue;
        }

        let (step_str, remaining) = split_at_slash(rest);
        rest = remaining;

        if step_str.is_empty() {
            continue;
        }

        if step_str == "text()" {
            steps.push(XPathStep::Text);
        } else if let Some(attr) = step_str.strip_prefix('@') {
            let (attr_name, pos_rest) = consume_name_and_predicate(attr);
            let (_, pos) = consume_position_predicate(pos_rest)?;
            steps.push(XPathStep::Attr(attr_name.to_string()));
            if let Some(p) = pos {
                steps.push(XPathStep::Position(p));
            }
        } else if step_str == "*" {
            steps.push(XPathStep::Wildcard);
        } else {
            // name[n]?
            let (name, pred_rest) = consume_name_and_predicate(step_str);
            let (_, pos) = consume_position_predicate(pred_rest)?;
            steps.push(XPathStep::Child(name.to_string()));
            if let Some(p) = pos {
                steps.push(XPathStep::Position(p));
            }
        }
    }

    Ok(steps)
}

fn split_at_slash(s: &str) -> (&str, &str) {
    match s.find('/') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

fn consume_name_and_predicate(s: &str) -> (&str, &str) {
    match s.find('[') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

fn consume_position_predicate(s: &str) -> Result<(&str, Option<usize>), DbError> {
    if !s.starts_with('[') {
        return Ok((s, None));
    }
    let end = s.find(']').ok_or(DbError::InvalidValue {
        reason: format!("unclosed '[' in XPath predicate: {s}"),
    })?;
    let inner = &s[1..end];
    let remaining = &s[end + 1..];
    let n: usize = inner.trim().parse().map_err(|_| DbError::InvalidValue {
        reason: format!("expected integer in XPath predicate, got: {inner}"),
    })?;
    Ok((remaining, Some(n)))
}

// ── XPath evaluation ──────────────────────────────────────────────────────────

/// Apply an absolute XPath `steps` to the document.
///
/// For absolute paths (`/root/child/...`) the first step is matched
/// against the document root element by name. If it matches, subsequent
/// steps navigate into children. This mirrors the behavior of
/// `eval_xpath` in `eval/functions/xml.rs`.
pub fn eval_xpath_nodes<'d>(
    doc: &'d roxmltree::Document<'d>,
    steps: &[XPathStep],
) -> Vec<roxmltree::Node<'d, 'd>> {
    if steps.is_empty() {
        return vec![doc.root_element()];
    }
    let root = doc.root_element();

    // First step: must match root element.
    let remaining = match &steps[0] {
        XPathStep::Child(name) => {
            if root.tag_name().name() != name.as_str() {
                return Vec::new();
            }
            &steps[1..]
        }
        XPathStep::Wildcard => &steps[1..],
        XPathStep::Descendant(name) => {
            // //name at the root — collect all descendants named `name`.
            let mut out = Vec::new();
            collect_descendants(&root, name, &mut out);
            if steps.len() > 1 {
                return eval_steps_on_nodes(out, &steps[1..]);
            }
            return out;
        }
        // Text/Attr at the root-level are unusual but handle gracefully.
        _ => steps,
    };

    if remaining.is_empty() {
        return vec![root];
    }
    eval_steps_on_nodes(vec![root], remaining)
}

/// Apply a column path to a context node (starting from it as root).
pub fn eval_column_path_on_node(
    node: roxmltree::Node<'_, '_>,
    steps: &[XPathStep],
) -> Option<String> {
    if steps.is_empty() {
        // No path → return text content of the context node
        let text: String = node
            .children()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        return if text.is_empty() { None } else { Some(text) };
    }

    // Check if final step is text() or @attr, which extract a scalar
    let (nav, terminal) = if matches!(steps.last(), Some(XPathStep::Text)) {
        (&steps[..steps.len() - 1], Some(XPathStep::Text))
    } else if matches!(steps.last(), Some(XPathStep::Attr(_))) {
        (&steps[..steps.len() - 1], steps.last().cloned())
    } else {
        (steps, None)
    };

    // For column paths, we start from the context node (not its parent)
    // If first step is a Child(name) matching the context node itself, start there
    // Otherwise treat context node as the parent and navigate children
    let start = determine_start_nodes(node, nav);
    let matched = if nav.is_empty() {
        start
    } else {
        eval_steps_on_nodes(start, nav)
    };

    let target = matched.into_iter().next()?;
    match &terminal {
        Some(XPathStep::Text) => {
            let text: String = target
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
        Some(XPathStep::Attr(name)) => target.attribute(name.as_str()).map(|s| s.to_string()),
        _ => {
            let text: String = target
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

fn determine_start_nodes<'d>(
    node: roxmltree::Node<'d, 'd>,
    steps: &[XPathStep],
) -> Vec<roxmltree::Node<'d, 'd>> {
    // If first step matches context node name → start with [node]
    // Otherwise → start from context node and search its children
    match steps.first() {
        Some(XPathStep::Child(name)) if node.tag_name().name() == name.as_str() => vec![node],
        Some(XPathStep::Wildcard) => vec![node],
        Some(XPathStep::Attr(_)) | Some(XPathStep::Text) => vec![node],
        _ => {
            // Search children
            node.children().filter(|n| n.is_element()).collect()
        }
    }
}

fn eval_steps_on_nodes<'d>(
    mut current: Vec<roxmltree::Node<'d, 'd>>,
    steps: &[XPathStep],
) -> Vec<roxmltree::Node<'d, 'd>> {
    for step in steps {
        match step {
            XPathStep::Child(name) => {
                let mut next = Vec::new();
                for node in &current {
                    // If node itself matches and we're navigating, take its matching children
                    // If node is the "starting" context, search its children
                    for child in node.children() {
                        if child.is_element() && child.tag_name().name() == name.as_str() {
                            next.push(child);
                        }
                    }
                }
                current = next;
            }
            XPathStep::Wildcard => {
                let mut next = Vec::new();
                for node in &current {
                    for child in node.children() {
                        if child.is_element() {
                            next.push(child);
                        }
                    }
                }
                current = next;
            }
            XPathStep::Descendant(name) => {
                let mut next = Vec::new();
                for node in &current {
                    collect_descendants(node, name, &mut next);
                }
                current = next;
            }
            XPathStep::Attr(_) | XPathStep::Text => {
                // Terminal steps — no further navigation
                break;
            }
            XPathStep::Position(n) => {
                // Keep only the nth element (1-based)
                let idx = n.saturating_sub(1);
                current = current.into_iter().nth(idx).into_iter().collect();
            }
        }
        if current.is_empty() {
            break;
        }
    }
    current
}

fn collect_descendants<'d>(
    node: &roxmltree::Node<'d, 'd>,
    name: &str,
    out: &mut Vec<roxmltree::Node<'d, 'd>>,
) {
    for child in node.children() {
        if child.is_element() {
            if child.tag_name().name() == name {
                out.push(child);
            }
            collect_descendants(&child, name, out);
        }
    }
}

// ── Compile ───────────────────────────────────────────────────────────────────

pub fn compile_xml_table(ast: &XmlTable) -> Result<XmlTableSpec, DbError> {
    let row_path = parse_xpath(&ast.row_path)?;
    let alias = ast.alias.clone().unwrap_or_else(|| "xmltable".to_string());

    let mut columns = Vec::new();
    for col_ast in &ast.columns {
        let path_str = col_ast.path.as_deref().unwrap_or(&col_ast.name);
        let path = if col_ast.ordinality {
            Vec::new()
        } else {
            parse_xpath(path_str)?
        };
        columns.push(XmlTableColumnSpec {
            name: col_ast.name.clone(),
            ty: col_ast.col_type.clone(),
            path,
            default_expr: col_ast.default_expr.clone(),
            not_null: col_ast.not_null,
            ordinality: col_ast.ordinality,
        });
    }

    Ok(XmlTableSpec {
        alias,
        row_path,
        columns,
    })
}

// ── Materialize ───────────────────────────────────────────────────────────────

/// Shred `xml_val` into rows using `spec`.
pub fn materialize_xml_table(
    spec: &XmlTableSpec,
    xml_val: &Value,
) -> Result<Vec<Vec<Value>>, DbError> {
    let xml_str = match xml_val {
        Value::Null => return Ok(Vec::new()),
        Value::Xml(s) | Value::Text(s) => s.as_str(),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "XML or TEXT".into(),
                got: other.variant_name().to_string(),
            })
        }
    };

    let doc = match roxmltree::Document::parse(xml_str) {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };

    let context_nodes = eval_xpath_nodes(&doc, &spec.row_path);

    let mut result: Vec<Vec<Value>> = Vec::new();
    for (row_idx, ctx) in context_nodes.iter().enumerate() {
        let mut row: Vec<Value> = Vec::with_capacity(spec.columns.len());
        for col in &spec.columns {
            if col.ordinality {
                row.push(Value::BigInt((row_idx + 1) as i64));
                continue;
            }

            let raw_opt = eval_column_for_node(*ctx, col);
            let val = match raw_opt {
                Some(s) => coerce_xml_value(s, &col.ty)?,
                None => match &col.default_expr {
                    Some(expr) => crate::eval::eval(expr, &[])?,
                    None if col.not_null => {
                        return Err(DbError::InvalidValue {
                            reason: format!(
                                "XMLTABLE column '{}' is NOT NULL but XPath matched nothing",
                                col.name
                            ),
                        })
                    }
                    None => Value::Null,
                },
            };
            row.push(val);
        }
        result.push(row);
    }
    Ok(result)
}

fn eval_column_for_node(ctx: roxmltree::Node<'_, '_>, col: &XmlTableColumnSpec) -> Option<String> {
    if col.path.is_empty() {
        // Default: text content of context node
        let text: String = ctx
            .children()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        return if text.is_empty() { None } else { Some(text) };
    }

    // The column path is relative to the context node
    eval_relative_xpath(ctx, &col.path)
}

/// Evaluate an XPath relative to a context node.
fn eval_relative_xpath(ctx: roxmltree::Node<'_, '_>, steps: &[XPathStep]) -> Option<String> {
    if steps.is_empty() {
        let text: String = ctx
            .children()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        return if text.is_empty() { None } else { Some(text) };
    }

    // Determine terminal step
    let (nav, terminal) = if matches!(steps.last(), Some(XPathStep::Text)) {
        (&steps[..steps.len() - 1], Some(XPathStep::Text))
    } else if matches!(steps.last(), Some(XPathStep::Attr(_))) {
        (&steps[..steps.len() - 1], steps.last().cloned())
    } else {
        (steps, None)
    };

    // Navigate from context node
    let mut current = vec![ctx];
    for step in nav {
        match step {
            XPathStep::Child(name) => {
                let mut next = Vec::new();
                for node in &current {
                    for child in node.children() {
                        if child.is_element() && child.tag_name().name() == name.as_str() {
                            next.push(child);
                        }
                    }
                }
                current = next;
            }
            XPathStep::Wildcard => {
                let mut next = Vec::new();
                for node in &current {
                    for child in node.children() {
                        if child.is_element() {
                            next.push(child);
                        }
                    }
                }
                current = next;
            }
            XPathStep::Attr(name) => {
                // Attribute access on current nodes
                return current
                    .into_iter()
                    .next()
                    .and_then(|n| n.attribute(name.as_str()).map(|s| s.to_string()));
            }
            XPathStep::Text => {
                return current.into_iter().next().map(|n| {
                    n.children()
                        .filter(|c| c.is_text())
                        .filter_map(|c| c.text())
                        .collect::<String>()
                });
            }
            XPathStep::Descendant(name) => {
                let mut next = Vec::new();
                for node in &current {
                    collect_descendants(node, name, &mut next);
                }
                current = next;
            }
            XPathStep::Position(n) => {
                let idx = n.saturating_sub(1);
                current = current.into_iter().nth(idx).into_iter().collect();
            }
        }
        if current.is_empty() {
            return None;
        }
    }

    let node = current.into_iter().next()?;
    match &terminal {
        Some(XPathStep::Text) => {
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
        Some(XPathStep::Attr(a)) => node.attribute(a.as_str()).map(|s| s.to_string()),
        _ => {
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

fn coerce_xml_value(s: String, ty: &DataType) -> Result<Value, DbError> {
    let text_val = Value::Text(s);
    match ty {
        DataType::Text => Ok(text_val),
        DataType::Xml => coerce(text_val, DataType::Xml, CoercionMode::Strict).map_err(|e| {
            DbError::InvalidValue {
                reason: e.to_string(),
            }
        }),
        other => coerce(text_val, other.clone(), CoercionMode::Permissive).map_err(|e| {
            DbError::InvalidValue {
                reason: e.to_string(),
            }
        }),
    }
}

// ── Correlated check ──────────────────────────────────────────────────────────

/// Returns true if the XMLTABLE uses outer-correlated expressions.
pub fn xmltable_is_correlated(ast: &XmlTable) -> bool {
    use crate::json_table::expr_has_outer_column_refs;
    ast.passing
        .iter()
        .any(|(e, _)| expr_has_outer_column_refs(e))
}

// ── Column metadata ───────────────────────────────────────────────────────────

pub fn column_metas_for_spec(spec: &XmlTableSpec) -> Vec<ColumnMeta> {
    spec.columns
        .iter()
        .map(|c| {
            let data_type = if c.ordinality {
                DataType::BigInt
            } else {
                c.ty.clone()
            };
            ColumnMeta::new(c.name.clone(), data_type, true, Some(spec.alias.clone()))
        })
        .collect()
}
