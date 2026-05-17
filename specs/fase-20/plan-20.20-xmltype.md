# Plan: 20.20 — XMLType hierarchical XML storage + XMLTABLE

Phase: 20 — Types + import/export
Task: 20.20 within the sprint
Spec: specs/fase-20/spec-20.20-xmltype.md
Status: in-progress

## Summary

Four compilable steps. Step 1 delivers the `Value::Xml` type end-to-end (codec,
coerce, DDL, wire, `xml_is_well_formed`) — the full storage layer in one commit.
Step 2 adds the five XML construction/query functions (`XMLELEMENT`, `XMLFOREST`,
`XMLROOT`, `XMLCONCAT`, `XMLQUERY`) as parser special forms + eval implementations.
Step 3 delivers `XMLTABLE` as a new `FromClause` variant with an internal minimal
XPath evaluator, mirroring the `json_table.rs` architecture. Step 4 closes the
subphase (integration tests, wire smoke, docs, memory). Every step passes
`cargo nextest run -p axiomdb-sql` before committing.

## Dependencies

Must be done first:
- [x] spec-20.20-xmltype.md approved
- [x] 20.19 (LTREE) complete — ColumnType=17 assigned; Xml=18 is next

Blocks (until this plan is done):
- [ ] Any future XMLTABLE-dependent specs

## Affected files

New files:
- `crates/axiomdb-sql/src/eval/functions/xml.rs` — XMLELEMENT/XMLFOREST/XMLROOT/XMLCONCAT/XMLQUERY + xml_is_well_formed eval
- `crates/axiomdb-sql/src/xml_table.rs` — XMLTABLE compiler + XPath evaluator + row materializer
- `crates/axiomdb-sql/src/parser/xml_table.rs` — XMLTABLE FROM clause parser
- `crates/axiomdb-sql/tests/integration_xml.rs` — integration tests
- `tools/xml_wire_smoke.py` — standalone wire smoke test (port 13307)

Modified files:
- `Cargo.toml` — add `roxmltree = "0.20"` to `[workspace.dependencies]`
- `crates/axiomdb-types/Cargo.toml` — add `roxmltree`
- `crates/axiomdb-sql/Cargo.toml` — add `roxmltree`
- `crates/axiomdb-types/src/value.rs` — `Value::Xml(String)` variant
- `crates/axiomdb-types/src/types.rs` — `DataType::Xml`
- `crates/axiomdb-types/src/codec.rs` — encode/decode/skip for `Value::Xml`
- `crates/axiomdb-types/src/coerce.rs` — Text→Xml, Xml→Text coercions
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Xml = 18`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `XML`/`XMLTYPE` type keywords; `DataType::Xml → ColumnType::Xml`
- `crates/axiomdb-sql/src/parser/expr.rs` — XMLELEMENT/XMLFOREST/XMLROOT/XMLCONCAT/XMLQUERY special-form parsers
- `crates/axiomdb-sql/src/ast.rs` — `FromClause::XmlTable`, `XmlTable`, `XmlTableColumn` AST structs; `Expr::XmlElement`, `Expr::XmlForest`, `Expr::XmlRoot`, `Expr::XmlConcat`, `Expr::XmlQuery` variants
- `crates/axiomdb-sql/src/analyzer_expr.rs` — bind new Expr variants
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — bind `FromClause::XmlTable`
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — dispatch `xml_is_well_formed` + new Expr variants
- `crates/axiomdb-sql/src/expr.rs` — eval new Expr variants in `eval_expr`
- `crates/axiomdb-network/src/mysql/result.rs` — `DataType::Xml` → 0xfd wire, `display_len=16777215`, charset=results_collation
- `docs/fase-20.md` — 20.20 section
- `docs/progreso.md` — 20.20 marked `[x] ✅`
- `docs-site/src/user-guide/sql-reference/data-types.md` — XML section
- `docs-site/src/user-guide/sql-reference/expressions.md` — XMLELEMENT/XMLTABLE examples
- `docs-site/src/development/roadmap.md` — last subphase = 20.20
- `memory/project_state.md`
- `memory/architecture.md`

---

## Step 1 — Core type: Value::Xml + ColumnType::Xml=18 + codec + coerce + DDL + wire + xml_is_well_formed

**Goal:** End-to-end `XML` column type — CREATE TABLE, INSERT, SELECT roundtrip working;
well-formedness enforced at coerce time via `roxmltree`; wire sends text not bytes.

**Files:**
- `Cargo.toml` (root workspace)
- `crates/axiomdb-types/Cargo.toml`
- `crates/axiomdb-sql/Cargo.toml`
- `crates/axiomdb-types/src/value.rs`
- `crates/axiomdb-types/src/types.rs`
- `crates/axiomdb-types/src/codec.rs`
- `crates/axiomdb-types/src/coerce.rs`
- `crates/axiomdb-catalog/src/schema_database.rs`
- `crates/axiomdb-sql/src/parser/ddl.rs`
- `crates/axiomdb-sql/src/eval/functions/mod.rs`
- `crates/axiomdb-network/src/mysql/result.rs`

**Approach:** Follow the exact same pattern as `Value::Ltree` (spec-20.19). Each file
change is mechanical — look at the Ltree variant and mirror it for Xml.

### Implementation outline

**`Cargo.toml` (workspace root)** — add:
```toml
roxmltree = "0.20"
```
under `[workspace.dependencies]`.

**`crates/axiomdb-types/Cargo.toml`** — add:
```toml
roxmltree = { workspace = true }
```

**`crates/axiomdb-sql/Cargo.toml`** — add same.

**`crates/axiomdb-types/src/value.rs`** — add variant after `Ltree`:
```rust
/// UTF-8 XML text, guaranteed well-formed at construction time.
Xml(String),
```
Update all match arms that cover `Ltree` to also handle `Xml`:
- `type_name()`: `Self::Xml(_) => "Xml"`
- `Display`: `Self::Xml(s) => write!(f, "{s}")`
- `infer_data_type()`: `Self::Xml(_) => DataType::Xml`

**`crates/axiomdb-types/src/types.rs`** — add `Xml` to `DataType` enum after `Ltree`.
Update `Display` / `type_name` / `from_column_type` where needed.

**`crates/axiomdb-types/src/codec.rs`** — mirror the Ltree u32-LE codec:
- `encoded_value_size`: `Value::Xml(s) => 4 + s.len()`
- `encode_value`: u32 LE length, then bytes
- `decode_value`: read 4-byte length, read bytes, `Value::Xml(String::from_utf8(...))`
- `skip_value` / `decode_row_masked`: read u32, skip N bytes
- `data_type_matches`: `(Value::Xml(_), DataType::Xml)`
- `infer_from_value`: `Value::Xml(_) => DataType::Xml`

**`crates/axiomdb-types/src/coerce.rs`** — add coercion cases:
```rust
// Text → Xml: validate with roxmltree
(Value::Text(s), DataType::Xml) => {
    validate_xml(&s)?;
    Ok(Value::Xml(s))
}
// Xml → Text: strip type wrapper
(Value::Xml(s), DataType::Text) => Ok(Value::Text(s)),
// Xml → Xml: identity
(Value::Xml(_), DataType::Xml) => Ok(val),
```
Add `validate_xml(s: &str) -> Result<(), DbError>` using `roxmltree::Document::parse()`.
Error type: `DbError::InvalidCoercion { from: "Text", to: "XML", reason: parse_error_msg }`.

**`crates/axiomdb-catalog/src/schema_database.rs`** — add:
```rust
Xml = 18,   // SQL XML/XMLTYPE column (Phase 20.20)
```
Update `TryFrom<u8>` upper bound from `18` to `19`. Update `From<ColumnType> for u8`.
Update column-type → DataType mapping in `schema_type_to_data_type()`.

**`crates/axiomdb-sql/src/parser/ddl.rs`** — add keyword arm in `parse_column_type()`:
```rust
Token::Ident(s) if s.eq_ignore_ascii_case("XML") || s.eq_ignore_ascii_case("XMLTYPE") => {
    (DataType::Xml, 0, false)
}
```
Add `DataType::Xml => Ok(ColumnType::Xml)` in `data_type_to_column_type()`.

**`crates/axiomdb-sql/src/eval/functions/mod.rs`** — add xml module + dispatch:
```rust
mod xml;
// In the function-name match:
"xml_is_well_formed" => xml::eval_is_well_formed(args, row),
```

**`crates/axiomdb-sql/src/eval/functions/xml.rs`** (new — Step 1 portion only):
```rust
use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use crate::expr::Expr;

pub(super) fn eval_is_well_formed(
    args: &[Expr],
    row: &[Value],
) -> Result<Value, DbError> {
    // expect 1 arg; NULL → NULL; Text → parse with roxmltree → 1/0
    ...
}

fn parse_xml(s: &str) -> bool {
    roxmltree::Document::parse(s).is_ok()
}
```

**`crates/axiomdb-network/src/mysql/result.rs`** — mirror Ltree:
```rust
// In value_to_mysql_packet (row serialization):
(DataType::Xml, Value::Xml(s)) => { /* length-prefix + bytes, same as Ltree */ }

// In column_charset():
DataType::Xml => results_collation.id,

// In datatype_to_mysql_type():
DataType::Xml => 0xfd,  // VAR_STRING

// In column_display_len():
DataType::Xml => 16_777_215,  // MEDIUMBLOB-size
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-types
./tools/vm.sh test -p axiomdb-sql -- integration_xml   # create table / insert / select roundtrip only
./tools/vm.sh clippy -p axiomdb-types -p axiomdb-sql -p axiomdb-catalog -p axiomdb-network
```

### Test to add (TDD seed — add before implementing)

```rust
// crates/axiomdb-sql/tests/integration_xml.rs
#[test]
fn xml_core_create_insert_select() { /* CREATE TABLE t (id INT, doc XML); INSERT ... SELECT */ }
#[test]
fn xml_cast_valid() { /* CAST('<root/>' AS XML) = Value::Xml */ }
#[test]
fn xml_cast_invalid() { /* CAST('<bad' AS XML) = InvalidCoercion */ }
#[test]
fn xml_cast_empty() { /* CAST('' AS XML) = InvalidCoercion */ }
#[test]
fn xml_to_text() { /* CAST('<a/>'::XML AS TEXT) = Value::Text */ }
#[test]
fn xml_is_well_formed_ok() { /* xml_is_well_formed('<a/>') = 1 */ }
#[test]
fn xml_is_well_formed_bad() { /* xml_is_well_formed('broken') = 0 */ }
#[test]
fn xml_is_well_formed_null() { /* xml_is_well_formed(NULL) = NULL */ }
```

### Commit

```
feat(fase-20): 20.20 step 1 — Value::Xml + ColumnType::Xml=18 + codec + coerce + DDL + wire + xml_is_well_formed
```

---

## Step 2 — XML construction functions (XMLELEMENT, XMLFOREST, XMLROOT, XMLCONCAT, XMLQUERY)

**Goal:** Five SQL/XML special-form functions parseable in SELECT lists; all produce
`Value::Xml` or `Value::Text`; XMLQUERY evaluates a basic XPath on a `roxmltree` document.

**Files:**
- `crates/axiomdb-sql/src/ast.rs` — new Expr variants for each function
- `crates/axiomdb-sql/src/parser/expr.rs` — special-form parsers
- `crates/axiomdb-sql/src/analyzer_expr.rs` — bind new Expr variants
- `crates/axiomdb-sql/src/expr.rs` — eval dispatch in `eval_expr`
- `crates/axiomdb-sql/src/eval/functions/xml.rs` — eval implementations

**Approach:** These functions have non-standard SQL syntax (bare `NAME`, `XMLATTRIBUTES`,
`PASSING`), so they must be parsed as special forms (like `CAST`, `EXTRACT`) rather
than ordinary function calls. Evaluation is pure string manipulation — no DOM needed
except for XMLQUERY (which uses `roxmltree` to parse + walk the document).

### AST additions (`ast.rs`)

```rust
/// XMLELEMENT(NAME tag [, XMLATTRIBUTES(v AS a, ...) ], content...)
pub struct XmlElementExpr {
    pub tag: String,
    pub attrs: Vec<(Expr, String)>,    // (value_expr, attr_name)
    pub content: Vec<Expr>,
}

/// XMLFOREST(expr AS name [, ...])
pub struct XmlForestExpr {
    pub items: Vec<(Expr, String)>,
}

/// XMLROOT(xml_expr, VERSION '1.0' [, STANDALONE YES|NO|NO VALUE])
pub struct XmlRootExpr {
    pub doc: Box<Expr>,
    pub version: String,
    pub standalone: Option<bool>,    // Some(true)=YES, Some(false)=NO, None=NO VALUE
}

/// XMLCONCAT(xml1 [, xml2, ...])
pub struct XmlConcatExpr {
    pub args: Vec<Expr>,
}

/// XMLQUERY(xpath PASSING xml_expr [RETURNING CONTENT])
pub struct XmlQueryExpr {
    pub xpath: String,
    pub doc: Box<Expr>,
}

// Add to the Expr enum:
// XmlElement(Box<XmlElementExpr>),
// XmlForest(Box<XmlForestExpr>),
// XmlRoot(Box<XmlRootExpr>),
// XmlConcat(Box<XmlConcatExpr>),
// XmlQuery(Box<XmlQueryExpr>),
```

### Parser outline (`parser/expr.rs`)

At the call-site where `CAST`, `EXTRACT`, etc. are parsed (typically a `match` on
the current token):

```rust
// parse_primary_expr or equivalent
Token::Ident(s) if s.eq_ignore_ascii_case("XMLELEMENT") => parse_xmlelement(p),
Token::Ident(s) if s.eq_ignore_ascii_case("XMLFOREST")  => parse_xmlforest(p),
Token::Ident(s) if s.eq_ignore_ascii_case("XMLROOT")    => parse_xmlroot(p),
Token::Ident(s) if s.eq_ignore_ascii_case("XMLCONCAT")  => parse_xmlconcat(p),
Token::Ident(s) if s.eq_ignore_ascii_case("XMLQUERY")   => parse_xmlquery(p),
```

**`parse_xmlelement`:**
```
XMLELEMENT ( NAME ident
    [, XMLATTRIBUTES ( expr AS ident [, ...] ) ]
    [, expr ... ]
)
```
Parse `NAME` as keyword (case-insensitive ident), then the tag name. Then while next
token is `,`: if ident = "XMLATTRIBUTES", parse attributes list; otherwise parse as
content expression.

**`parse_xmlforest`:**
```
XMLFOREST ( expr AS ident [, ...] )
```

**`parse_xmlroot`:**
```
XMLROOT ( expr , VERSION string_literal [, STANDALONE YES|NO|NO VALUE] )
```

**`parse_xmlconcat`:**
```
XMLCONCAT ( expr [, expr ...] )
```

**`parse_xmlquery`:**
```
XMLQUERY ( string_literal PASSING expr [RETURNING CONTENT] )
```

### Eval implementations (`eval/functions/xml.rs`)

**`eval_xmlelement(tag, attrs, content, row)`:**
1. Build `result = format!("<{tag}")`.
2. For each attr `(v_expr, a_name)`: eval v_expr; if NULL skip; else `result += format!(" {}=\"{}\"", a_name, escape_attr(text))`.
3. `result += ">"`.
4. For each content expr: eval; if NULL skip; if `Value::Xml(s)` append raw; else append `escape_text(to_string(v))`.
5. `result += format!("</{tag}>")`.
6. Validate tag name is `[A-Za-z_][A-Za-z0-9_.-]*`; error if not.
7. Return `Value::Xml(result)`.

**`eval_xmlforest(items, row)`:**
- For each `(expr, name)`: eval; if NULL skip; else `<name>escape_text(val)</name>`.
- Concat all; return `Value::Xml`.

**`eval_xmlroot(doc_expr, version, standalone, row)`:**
- Eval doc_expr → get XML string.
- Strip existing `<?xml ...?>` declaration if present.
- Prepend `<?xml version="VERSION" [standalone="yes/no"]?>`.
- Return `Value::Xml`.

**`eval_xmlconcat(args, row)`:**
- Eval all; skip NULLs; concat raw XML strings.
- If all NULL → `Value::Null`.
- Return `Value::Xml`.

**`eval_xmlquery(xpath, doc_expr, row)`:**
- Eval doc_expr → get XML text (or NULL → return NULL).
- Parse with `roxmltree::Document::parse(&text)` → error → return NULL (not error).
- Walk XPath using the same minimal evaluator written in Step 3 (or inline a simplified version here that only handles `/root/elem/text()` and `@attr` for XMLQUERY).
- Return first match as `Value::Text`, or `Value::Null` if no match.

**Helper functions:**
```rust
fn escape_attr(s: &str) -> String  // & → &amp;  < → &lt;  " → &quot;
fn escape_text(s: &str) -> String  // & → &amp;  < → &lt;  > → &gt;
fn strip_xml_decl(s: &str) -> &str // strip leading <?xml...?> if present
fn validate_xml_name(s: &str) -> Result<(), DbError>  // [A-Za-z_][A-Za-z0-9_.-]*
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql -- integration_xml
./tools/vm.sh clippy -p axiomdb-sql
```

### Tests to add

```rust
#[test] fn xmlelement_simple()      { /* XMLELEMENT(NAME 'a', 'hello') = '<a>hello</a>' */ }
#[test] fn xmlelement_attrs()       { /* XMLELEMENT(NAME 'a', XMLATTRIBUTES('x' AS 'id'), 'b') */ }
#[test] fn xmlelement_escaping()    { /* XMLELEMENT(NAME 'a', '<') = '<a>&lt;</a>' */ }
#[test] fn xmlelement_null_content(){ /* NULL content omitted */ }
#[test] fn xmlelement_bad_tag()     { /* XMLELEMENT(NAME 'bad name') = InvalidValue */ }
#[test] fn xmlforest_basic()        { /* XMLFOREST(1 AS "id", 'bob' AS "name") */ }
#[test] fn xmlforest_null_omitted() { /* NULL arg skipped */ }
#[test] fn xmlroot_basic()          { /* XMLROOT('<a/>'::XML, VERSION '1.0', STANDALONE YES) */ }
#[test] fn xmlroot_strips_existing_decl() { /* strips <?xml...?> before prepending */ }
#[test] fn xmlconcat_basic()        { /* XMLCONCAT('<a/>'::XML, '<b/>'::XML) = '<a/><b/>' */ }
#[test] fn xmlconcat_null_skipped() { /* NULL arg skipped */ }
#[test] fn xmlconcat_all_null()     { /* all NULL → NULL */ }
#[test] fn xmlquery_basic()         { /* XMLQUERY('/root/a/text()' PASSING '<root><a>hi</a></root>'::XML) = 'hi' */ }
#[test] fn xmlquery_no_match()      { /* XPath no match → NULL */ }
#[test] fn xmlquery_null_doc()      { /* NULL doc → NULL */ }
```

### Commit

```
feat(fase-20): 20.20 step 2 — XMLELEMENT, XMLFOREST, XMLROOT, XMLCONCAT, XMLQUERY
```

---

## Step 3 — XMLTABLE table-valued function

**Goal:** `XMLTABLE(row_xpath PASSING xml_expr COLUMNS ...)` parses as a
`FromClause::XmlTable`, binds in the analyzer, and materializes rows via
the internal XPath evaluator. Mirrors the `json_table.rs` architecture exactly.

**Files:**
- `crates/axiomdb-sql/src/ast.rs` — `FromClause::XmlTable`, `XmlTable`, `XmlTableColumn`
- `crates/axiomdb-sql/src/parser/xml_table.rs` (new) — XMLTABLE FROM clause parser
- `crates/axiomdb-sql/src/parser/mod.rs` — call `parse_xml_table` from FROM clause
- `crates/axiomdb-sql/src/analyzer_stmt.rs` — bind `FromClause::XmlTable`
- `crates/axiomdb-sql/src/xml_table.rs` (new) — `XmlTableSpec` + XPath evaluator + materializer
- `crates/axiomdb-sql/src/executor/select.rs` or `select_core.rs` — dispatch on `FromClause::XmlTable`
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — explain branch for XmlTable

**Approach:** Exactly mirrors `json_table.rs`. The XPath evaluator is a pure Rust
recursive walker over `roxmltree::Node` — no external crate beyond `roxmltree`.

### AST (`ast.rs`)

```rust
/// Phase 20.20 — XMLTABLE FROM clause (SQL/XML standard)
#[derive(Debug, Clone, PartialEq)]
pub struct XmlTable {
    pub row_path: String,
    /// PASSING bindings: (alias_name, expr). Typically one unnamed binding.
    pub passing: Vec<(String, Expr)>,
    pub columns: Vec<XmlTableColumn>,
    /// Table alias (`AS x`). Required.
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XmlTableColumn {
    pub name: String,
    pub col_type: DataType,
    pub path: Option<String>,      // None → use column name as path
    pub default_expr: Option<Expr>,
    pub not_null: bool,
}

// Add to FromClause:
/// Phase 20.20 — `XMLTABLE(row_path PASSING xml_expr COLUMNS ...) [AS alias]`
XmlTable(Box<XmlTable>),
```

### Parser (`parser/xml_table.rs`)

```
XMLTABLE (
    row_xpath_string
    PASSING expr [AS name] [, ...]
    COLUMNS
        col_name col_type [PATH col_xpath_string] [DEFAULT expr] [NOT NULL]
        [, ...]
) [AS alias]
```

Parse steps:
1. Consume `XMLTABLE` `(`.
2. Parse row XPath: expect `Token::StringLiteral`.
3. Consume `PASSING`.
4. Parse passing expr (and optional `AS name`); collect into `Vec<(String, Expr)>`.
5. Consume `COLUMNS`.
6. Parse column list (comma-separated until `)`) — each: ident, type, optional `PATH string`, optional `DEFAULT expr`, optional `NOT NULL`.
7. Consume `)`. Parse optional `AS alias`.

### `xml_table.rs` — XmlTableSpec + XPath evaluator

```rust
/// Compiled XMLTABLE.
pub struct XmlTableSpec {
    pub alias: String,
    pub row_path: Vec<XPathStep>,
    pub columns: Vec<XmlTableColumnSpec>,
    pub passing: Vec<(String, Expr)>,
}

pub struct XmlTableColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub path: Vec<XPathStep>,
    pub default_expr: Option<Expr>,
    pub not_null: bool,
    pub slot: usize,
}

/// Minimal XPath step.
pub enum XPathStep {
    Root,              // `/` at position 0
    Child(String),     // `elem` — named child element
    Wildcard,          // `*`    — any child element
    Descendant(String),// `//elem`
    Attr(String),      // `@attr`
    Text,              // `text()`
    Position(usize),   // `[n]` (1-based)
    Self_,             // `.`
}

/// Parse an XPath string into steps.
pub fn parse_xpath(xpath: &str) -> Result<Vec<XPathStep>, DbError>;

/// Apply row path to a document; return one context node per match.
pub fn eval_row_path<'d>(
    doc: &'d roxmltree::Document<'d>,
    path: &[XPathStep],
) -> Vec<roxmltree::Node<'d, 'd>>;

/// Apply a column path to a context node; return string value or None.
pub fn eval_column_path<'d>(
    node: roxmltree::Node<'d, 'd>,
    path: &[XPathStep],
) -> Option<String>;

/// Compile `ast::XmlTable` → `XmlTableSpec`.
pub fn compile_xml_table(jt: &ast::XmlTable) -> Result<XmlTableSpec, DbError>;

/// Materialize all rows from a document value.
pub fn materialize(
    spec: &XmlTableSpec,
    xml_val: Value,
    passing_vals: &[(String, Value)],
    runner: &SubqueryRunner<'_>,
    outer_row: &[Value],
) -> Result<Vec<Vec<Value>>, DbError>;
```

**XPath parse algorithm:**
- Split on `/` but handle `//elem` and `@attr` and `text()` specially.
- Consecutive `/` at start → `XPathStep::Root`.
- `//elem` → `XPathStep::Descendant(elem)`.
- `@attr` → `XPathStep::Attr(attr)`.
- `text()` → `XPathStep::Text`.
- `*` → `XPathStep::Wildcard`.
- `[n]` suffix on preceding step → append `XPathStep::Position(n)`.
- Plain ident → `XPathStep::Child(ident)`.

**`eval_column_path` algorithm:**
- Start with `[context_node]` as current nodes.
- For each step:
  - `Child(name)` → collect children named `name` from all current nodes.
  - `Wildcard` → collect all element children.
  - `Descendant(name)` → depth-first collect all descendants named `name`.
  - `Attr(a)` → return attribute `a` value of first current node as string.
  - `Text` → return concatenated text children of first current node.
  - `Position(n)` → keep only the `n-1`th element (1-based) from current.
  - `Root` → jump to document root (only valid at position 0).
  - `Self_` → unchanged.
- Return `Some(text_of_first_match)` or `None`.

### Executor integration

In `select_core.rs` (or wherever `FromClause::JsonTable` is dispatched):
```rust
FromClause::XmlTable(xt) => {
    let spec = compile_xml_table(xt)?;
    let xml_val = eval_passing(&spec.passing, ...)?;
    let rows = materialize(&spec, xml_val, ...)?;
    // wrap into ScanCursor similar to JsonTable
}
```

Add explain branch in `exec_explain.rs` similarly to `JsonTable`.

### Tests to add

```rust
#[test] fn xmltable_single_row_attr()        { /* /order/item PASSING ..., @id */ }
#[test] fn xmltable_multi_row()              { /* multiple rows from row XPath */ }
#[test] fn xmltable_text_content()           { /* col PATH 'name/text()' */ }
#[test] fn xmltable_missing_col_null()       { /* missing PATH → NULL */ }
#[test] fn xmltable_default_used()           { /* DEFAULT expr when path misses */ }
#[test] fn xmltable_not_null_violation()     { /* NOT NULL + miss → ConstraintViolation */ }
#[test] fn xmltable_descendant_search()      { /* //elem path */ }
#[test] fn xmltable_positional_predicate()   { /* [2] picks second element */ }
#[test] fn xmltable_join_with_real_table()   { /* XMLTABLE in JOIN */ }
#[test] fn xmltable_zero_rows()             { /* XPath matches nothing → 0 rows */ }
#[test] fn xmltable_xml_declaration_doc()   { /* doc with <?xml...?> header */ }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql -- integration_xml
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): 20.20 step 3 — XMLTABLE TVF with XPath evaluator
```

---

## Step 4 — Integration tests + wire smoke + closing protocol

**Goal:** Complete all integration tests (≥ 30 total), wire smoke, docs updates,
memory updates, and the standard closing protocol.

**Files:** (see closing-protocol list in CLAUDE.md)

### Full integration test suite target

All tests in `crates/axiomdb-sql/tests/integration_xml.rs`:

```
Step 1 (core):   xml_core_create_insert_select, xml_cast_valid, xml_cast_invalid,
                 xml_cast_empty, xml_to_text, xml_is_well_formed_ok,
                 xml_is_well_formed_bad, xml_is_well_formed_null,
                 xml_null_propagation, xml_xmltype_keyword_alias
Step 2 (funcs):  xmlelement_simple, xmlelement_attrs, xmlelement_escaping,
                 xmlelement_null_content, xmlelement_bad_tag, xmlforest_basic,
                 xmlforest_null_omitted, xmlroot_basic, xmlroot_strips_existing_decl,
                 xmlconcat_basic, xmlconcat_null_skipped, xmlconcat_all_null,
                 xmlquery_basic, xmlquery_no_match, xmlquery_null_doc,
                 xmlquery_attr_path
Step 3 (table):  xmltable_single_row_attr, xmltable_multi_row, xmltable_text_content,
                 xmltable_missing_col_null, xmltable_default_used,
                 xmltable_not_null_violation, xmltable_descendant_search,
                 xmltable_positional_predicate, xmltable_join_with_real_table,
                 xmltable_zero_rows, xmltable_xml_declaration_doc
```

### Wire smoke (`tools/xml_wire_smoke.py`)

Standalone script on port 13307 (same pattern as `tools/ltree_wire_smoke.py`):
- [20.20 xml] XML column roundtrip
- [20.20 xml] XMLELEMENT via SELECT
- [20.20 xml] XMLFOREST via SELECT
- [20.20 xml] XMLQUERY via SELECT
- [20.20 xml] XMLTABLE shreds document

### Closing protocol

```bash
# 1. workspace test
./tools/vm.sh test --workspace

# 2. clippy
./tools/vm.sh clippy --workspace -- -D warnings

# 3. fmt
cargo fmt --check

# 4. wire smoke
limactl shell axiomdb -- python3 /path/to/xml_wire_smoke.py

# 5. docs: docs/fase-20.md — add 20.20 section
# 6. docs: docs/progreso.md — 20.20 [x] ✅

# 7. docs-site: data-types.md, expressions.md, roadmap.md

# 8. memory: project_state.md, architecture.md
```

### Verification against spec done criteria

- [x] `CREATE TABLE t (id INT, doc XML)` works
- [x] INSERT + SELECT XML column roundtrip
- [x] `CAST('<bad' AS XML)` raises `InvalidCoercion`
- [x] `xml_is_well_formed` returns 1/0/NULL
- [x] XMLELEMENT + XMLFOREST + XMLROOT + XMLCONCAT work in SELECT list
- [x] XMLQUERY returns first match as TEXT or NULL
- [x] XMLTABLE shreds multi-row document correctly
- [x] `cargo nextest run --workspace` 100% pass
- [x] `cargo clippy --workspace -- -D warnings` clean
- [x] `cargo fmt --check` clean
- [x] Wire smoke passes

### Final commit

```
feat(fase-20): complete 20.20 XMLType — core + XML functions + XMLTABLE

Implements specs/fase-20/spec-20.20-xmltype.md
Plan: specs/fase-20/plan-20.20-xmltype.md
Tests: ≥30 integration tests + 5 wire smoke assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| roxmltree version conflict with workspace | low | pin `0.20` in workspace.dependencies; double-check semver |
| XMLELEMENT/XMLFOREST parsing conflicts with existing ident handling | medium | parse before general function-call path; test in Step 2 before wiring |
| XPath `//elem` descendant perf on large docs | low | tree walk is bounded by document size; no index needed here |
| XMLTABLE analyzer binding complexity | medium | mirror json_table.rs exactly — same column-slot model |
| `parse_xml_decl` stripping in XMLROOT | low | simple `starts_with("<?xml")` + find `?>` boundary |

## Rollback plan

1. `git reset --hard <commit before Step 1>` — or —
2. Branch `abandoned/plan-20.20-xmltype-<date>`
3. Revert spec status to `draft`

## Estimated effort

Total: ~6-8 hours
- Step 1 (core type): ~1.5h — mechanical mirrors of Ltree pattern
- Step 2 (XML functions): ~2h — parser special forms + eval string building + XMLQUERY XPath
- Step 3 (XMLTABLE): ~2.5h — XPath evaluator + json_table.rs mirroring
- Step 4 (tests + close): ~1h — integration tests already seeded in Steps 1-3
