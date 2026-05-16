# Spec: 20.20 — XMLType hierarchical XML storage + XMLTABLE

Phase: 20 — Types + import/export
Task: 20.20 within the sprint
Status: approved

## Context

AxiomDB Phase 20 adds user-defined and complex types. Phase 20.19 added `LTREE`;
Phase 20.20 adds `XML` (`XMLTYPE`), a native XML document type modelled after
PostgreSQL's `xml` type. This is critical for Oracle/PostgreSQL migrations involving
SOAP web services, EDI (EDIFACT), HL7 healthcare messages, SWIFT financial messages,
and any system that exchanges XML documents through the database. MySQL lacks native
XMLType; AxiomDB gains a concrete compatibility advantage.

The implementation is split into three logical steps that can be planned and committed
independently but are specified together here as a single contract:

- **Step 1** — Core type (`Value::Xml`, `ColumnType::Xml=18`, codec, DDL, wire)
- **Step 2** — XML construction functions (`XMLELEMENT`, `XMLFOREST`, `XMLROOT`,
  `XMLCONCAT`, `xml_is_well_formed`, `XMLQUERY`)
- **Step 3** — `XMLTABLE` table-valued function (XPath document shredding)

## Goal

Store validated XML documents in typed columns, construct XML from relational data,
and shred XML documents into relational rows via `XMLTABLE`.

## Non-goals

- XSD / RELAX-NG schema validation — deferred to 20.20b
- Full XPath 1.0 (axes `ancestor::`, `preceding-sibling::`, `following-sibling::`,
  predicates on expressions `[.='val']`, `[position()>1]`) — deferred to 20.20b
- XQuery — deferred to 20.20b (XQuery is a complete language)
- Namespace-aware XPath (xmlns: prefixes) — deferred to 20.20b
- XML indexes (GIN/inverted index on element paths) — deferred to a later phase
- Mutable DOM operations (`XMLMODIFY`, `UPDATEXML`) — deferred to 20.20b
- XMLAGG aggregate — deferred to 20.20b

---

## Step 1 — Core type

### Public API

```rust
// axiomdb-types/src/value.rs
pub enum Value {
    // ... existing variants ...
    Xml(String),   // UTF-8 XML text, validated well-formed at construction time
}

// axiomdb-types/src/types.rs
pub enum DataType {
    // ...
    Xml,
}

// axiomdb-catalog/src/schema_database.rs + schema_aggregate.rs
pub enum ColumnType {
    // ...
    Xml = 18,
}
```

### Semantics

**Validation:** A `Value::Xml` is guaranteed to contain a well-formed XML document
(or fragment — a sequence of XML nodes). Validation uses `roxmltree`'s parser.
An empty string is NOT valid XML.

**On-disk codec** — identical pattern to `Value::Ltree`:
```
offset  size  field
0       4     length  u32 LE, byte count of the UTF-8 text that follows
4       N     text    UTF-8 XML bytes
```
Codec functions: `encode_row`, `decode_row`, `decode_row_masked` in `axiomdb-types/src/codec.rs`.

**Coercions:**
- `Text → Xml`: parse with `roxmltree`; return `DbError::InvalidCoercion` if not well-formed.
- `Xml → Text`: strip the type wrapper, return raw text.
- `Xml → Xml`: identity.
- `Null → Xml`: NULL propagates.

**DDL keywords:** both `XML` and `XMLTYPE` are accepted as column type names.
They map to the same `DataType::Xml` / `ColumnType::Xml`.

**Wire:** `DataType::Xml` → MySQL type `0xfd` (`VAR_STRING`),
`display_len = 16777215` (MEDIUMBLOB-size limit), charset = `results_collation.id`
(not binary 63 — pymysql must get text, not bytes).

**Scalar function `xml_is_well_formed(text) → Int`:**
- Returns `1` if the text argument is well-formed XML, `0` otherwise.
- Returns `NULL` if argument is NULL.
- Uses `roxmltree::Document::parse()` to test; does not store the result.

### Error cases

| Situation | Error |
|-----------|-------|
| `'broken<'::XML` | `DbError::InvalidCoercion { from: "Text", to: "XML", reason: "..." }` |
| `''::XML` | `DbError::InvalidCoercion` — empty string is not valid XML |
| `xml_is_well_formed(NULL)` | `Value::Null` |

### Edge cases

- [ ] Empty string rejected
- [ ] XML with BOM (`\xEF\xBB\xBF`) — accepted (roxmltree handles BOM)
- [ ] Very large documents (>1 MB) — accepted; no size cap beyond u32 codec limit (4 GB)
- [ ] XML declaration `<?xml version="1.0" encoding="UTF-8"?>` — accepted
- [ ] XML fragments (no single root element, e.g., `<a/><b/>`) — accepted (valid for XMLFOREST output)
- [ ] NULL column — propagates correctly through all operations
- [ ] Non-UTF-8 encodings (`encoding="ISO-8859-1"`) — accepted as-is (stored as received; re-encoding deferred)

### Done criteria — Step 1

- [ ] `CREATE TABLE t (id INT, doc XML)` works
- [ ] `INSERT INTO t VALUES (1, '<root><a>1</a></root>')` stores document
- [ ] `SELECT doc FROM t` retrieves the text unchanged
- [ ] `SELECT '<bad'::XML` raises `InvalidCoercion`
- [ ] `SELECT CAST('<root/>' AS XML)` works
- [ ] `SELECT CAST('<root/>'::XML AS TEXT)` returns `'<root/>'`
- [ ] `SELECT xml_is_well_formed('<a/>')` returns `1`
- [ ] `SELECT xml_is_well_formed('broken')` returns `0`
- [ ] `SELECT xml_is_well_formed(NULL)` returns NULL
- [ ] Wire: pymysql receives a `str`, not `bytes`
- [ ] `cargo test -p axiomdb-sql` passes; `cargo clippy` clean

---

## Step 2 — XML construction functions

### Public API (SQL)

```sql
-- Construct a single XML element
XMLELEMENT(NAME tag_name [, XMLATTRIBUTES(expr AS attr_name [, ...]) ], content_expr [, ...])
  → XML

-- Construct a sequence of XML elements (one per named argument)
XMLFOREST(expr AS name [, expr AS name, ...])
  → XML

-- Prepend XML declaration to a document
XMLROOT(xml_expr, VERSION '1.0' [, STANDALONE YES|NO|NO VALUE])
  → XML

-- Concatenate XML fragments
XMLCONCAT(xml_expr [, xml_expr, ...])
  → XML

-- Apply simple XPath and return first match as text
XMLQUERY(xpath_string PASSING xml_expr [RETURNING CONTENT])
  → TEXT | NULL
```

### Semantics

**`XMLELEMENT(NAME name, XMLATTRIBUTES(v1 AS a1, v2 AS a2), c1, c2)`**
1. Build the opening tag: `<name>`.
2. Append each attribute: ` a1="v1_escaped" a2="v2_escaped"`.
3. Append each content argument converted to text (XML-escaped if scalar; raw if already `Xml`).
4. Append closing tag: `</name>`.
5. Return `Value::Xml(result)`.

Attribute value escaping: `&` → `&amp;`, `<` → `&lt;`, `"` → `&quot;`.
Content escaping (for scalar values): `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`.

**`XMLFOREST(v1 AS n1, v2 AS n2, ...)`**
- Emit `<n1>v1_escaped</n1><n2>v2_escaped</n2>...`.
- Arguments with NULL values are silently omitted (PostgreSQL behavior).
- Returns `Value::Xml` with a fragment (possibly empty if all NULL).

**`XMLROOT(xml, VERSION '1.0', STANDALONE YES)`**
- Prepends `<?xml version="1.0" standalone="yes"?>` (or `standalone="no"`, or no standalone attr).
- If xml already starts with `<?xml`, strip the existing declaration first.

**`XMLCONCAT(xml1, xml2, ...)`**
- Concatenate the raw text of all Xml arguments.
- NULL arguments are skipped (PostgreSQL behavior).
- Returns NULL only if ALL arguments are NULL.

**`XMLQUERY(xpath PASSING xml_val)`**
- Apply the XPath (see Step 3 XPath subset) to the document.
- Return the **string value** of the first matching node as `Value::Text`.
- Return `Value::Null` if no match or if `xml_val` is NULL.
- `RETURNING CONTENT` keyword is parsed but ignored (always returns content).

**Parser note:** `XMLELEMENT`, `XMLFOREST`, `XMLROOT`, `XMLCONCAT`, `XMLQUERY` are parsed as
special-form function calls (like `EXTRACT`, `CAST`) because their argument syntax
(bare `NAME`, `XMLATTRIBUTES`, `PASSING`) is not standard function call syntax.

### Error cases

| Situation | Error |
|-----------|-------|
| `XMLELEMENT(NAME 'bad name')` — spaces in tag | `DbError::InvalidValue { reason: "XML element name ..." }` |
| `XMLELEMENT()` — zero args | `DbError::TypeMismatch` |
| `XMLFOREST()` — zero args | `DbError::TypeMismatch` |

Tag name validation: must match `[A-Za-z_][A-Za-z0-9_.-]*` (simplified NCName).

### Edge cases

- [ ] NULL content in XMLELEMENT → omit (produce `<tag/>` or `<tag></tag>`)
- [ ] NULL attribute in XMLATTRIBUTES → attribute omitted
- [ ] NULL arg in XMLFOREST → element omitted
- [ ] NULL arg in XMLCONCAT → skipped
- [ ] All-NULL XMLCONCAT → NULL
- [ ] XMLELEMENT with an Xml-typed content arg → raw insert (no double-escaping)
- [ ] XMLROOT with no standalone → no `standalone` attribute
- [ ] XMLQUERY on NULL doc → NULL
- [ ] XMLQUERY XPath no match → NULL

### Done criteria — Step 2

- [ ] `SELECT XMLELEMENT(NAME 'a', 'hello')` → `'<a>hello</a>'`
- [ ] `SELECT XMLELEMENT(NAME 'a', XMLATTRIBUTES('x' AS 'id'), 'body')` → `'<a id="x">body</a>'`
- [ ] `SELECT XMLELEMENT(NAME 'a', '<')` → `'<a>&lt;</a>'` (escaped)
- [ ] `SELECT XMLFOREST(1 AS "id", 'bob' AS "name")` → `'<id>1</id><name>bob</name>'`
- [ ] `SELECT XMLFOREST(NULL AS "x", 'y' AS "y")` → `'<y>y</y>'` (NULL omitted)
- [ ] `SELECT XMLROOT('<a/>'::XML, VERSION '1.0', STANDALONE YES)` → has `<?xml ...?>`
- [ ] `SELECT XMLCONCAT('<a/>'::XML, '<b/>'::XML)` → `'<a/><b/>'`
- [ ] `SELECT XMLCONCAT('<a/>'::XML, NULL::XML)` → `'<a/>'`
- [ ] `SELECT XMLQUERY('/root/a/text()' PASSING '<root><a>hello</a></root>'::XML)` → `'hello'`
- [ ] `SELECT XMLQUERY('/root/missing' PASSING '<root/>'::XML)` → NULL
- [ ] `cargo test -p axiomdb-sql` passes; clippy clean

---

## Step 3 — XMLTABLE table-valued function

### Public API (SQL)

```sql
XMLTABLE(
    row_xpath
    PASSING xml_expr
    COLUMNS
        col_name col_type PATH col_xpath [DEFAULT expr] [NOT NULL],
        ...
)
```

Examples:
```sql
-- Shred an XML column into rows
SELECT x.id, x.name, x.price
FROM orders,
     XMLTABLE('/order/item' PASSING orders.xml_data
              COLUMNS
                id    INT     PATH '@id',
                name  TEXT    PATH 'name/text()',
                price DECIMAL PATH 'price/text()') AS x;

-- Literal document
SELECT * FROM XMLTABLE('/root/row' PASSING '<root><row id="1"/><row id="2"/></root>'::XML
                       COLUMNS id INT PATH '@id');
```

### Supported XPath subset

**Row path and column paths use the same evaluator.**

Supported steps (evaluated left-to-right, starting from context node):

| Pattern | Meaning |
|---------|---------|
| `/` | root of document (if first char of row path) |
| `elem` | child elements named `elem` |
| `*` | all child elements |
| `.` | current node (for `text()` on self) |
| `//elem` | any descendant named `elem` (depth-first search) |
| `@attr` | attribute value of current element |
| `text()` | concatenated text content of current element |
| `[n]` | positional predicate (1-based integer literal only) |

**Not supported** (return empty result, no error):
- `..` parent axis
- `ancestor::`, `following::`, `preceding::` axes
- Predicate expressions beyond `[n]` (e.g., `[@id='x']`, `[.='val']`)
- Namespace prefixes (`ns:elem`)
- `node()`, `comment()`, `processing-instruction()` node tests

**XPath evaluation rules:**
1. Row path is evaluated against the document root. Each matched node becomes one output row.
2. Column path is evaluated against the per-row context node.
3. `@attr` returns the attribute string value.
4. `text()` returns concatenation of all direct text children.
5. For element steps: returns the first match (for column paths; row path returns all matches).
6. If a column path matches no node and `DEFAULT expr` is provided: use the default.
7. If a column path matches no node and no DEFAULT: return NULL.
8. `NOT NULL` constraint on a column path that evaluates to NULL raises `DbError::ConstraintViolation`.

### Parser changes

`XMLTABLE(...)` is parsed as a special table reference in the `FROM` clause, similar to
`JSON_TABLE(...)`. It introduces a new `FromClause::XmlTable(Box<XmlTable>)` variant.

AST:
```rust
pub struct XmlTable {
    pub row_path: String,        // XPath expression for row iteration
    pub passing: Vec<(String, Expr)>, // PASSING bindings (name → expr)
    pub columns: Vec<XmlTableColumn>,
    pub alias: String,
}

pub struct XmlTableColumn {
    pub name: String,
    pub col_type: DataType,
    pub path: Option<String>,    // None = use column name as path
    pub default_expr: Option<Expr>,
    pub not_null: bool,
}
```

### Semantics

`XMLTABLE` materializes the full result set into a `Vec<Row>` before the outer query
runs (same model as `JSON_TABLE`). No streaming — documents are assumed to fit in memory.

The PASSING clause binds names to expressions that can be used within the row_path
and column paths. If PASSING is absent, the document is the only input.

Column type coercion follows the same rules as INSERT: the extracted text is coerced
to `col_type` using `coerce(Value::Text(s), col_type, CoercionMode::Implicit)`.

### Error cases

| Situation | Error |
|-----------|-------|
| `PASSING` value is not `Xml` or `Text` | `DbError::TypeMismatch` |
| Invalid XML in PASSING | `DbError::InvalidCoercion` |
| `NOT NULL` column path matches no node | `DbError::ConstraintViolation` |
| Type coercion of extracted text fails | `DbError::InvalidCoercion` |

### Edge cases

- [ ] No rows match row XPath → zero rows returned (no error)
- [ ] Column path matches no node, no DEFAULT → NULL value in that column
- [ ] Column path `@id` on a node with no `id` attribute → NULL
- [ ] Nested XML in a column path → return full serialized XML text as TEXT
- [ ] Multiple matches for a column path → first match wins
- [ ] Document with XML declaration `<?xml ...?>` → row path evaluated on document root
- [ ] NULL PASSING expression → column returns NULL for every row

### Done criteria — Step 3

- [ ] `SELECT * FROM XMLTABLE('/order/item' PASSING '<order><item id="1"><name>A</name></item></order>'::XML COLUMNS id INT PATH '@id', name TEXT PATH 'name/text()')` returns `(1, 'A')`
- [ ] Row XPath `/root/row` matches multiple rows → each emits one output row
- [ ] Column path `@attr` extracts attribute correctly
- [ ] Column path `text()` extracts element text content
- [ ] Missing column path with DEFAULT → default value used
- [ ] Missing column path without DEFAULT → NULL
- [ ] NOT NULL missing → `DbError::ConstraintViolation`
- [ ] `//elem` descendant search works
- [ ] `[n]` positional predicate selects nth element
- [ ] XMLTABLE in JOIN with real table works
- [ ] `cargo test -p axiomdb-sql` passes; clippy clean

---

## On-disk format

Same as `Value::Ltree`:
```
offset  size  field
0       4     length    u32 little-endian, byte count N of the UTF-8 payload
4       N     payload   UTF-8-encoded XML text
```

Skip logic (in `decode_row_masked`): read 4-byte length, skip N bytes.

## Performance budget

| Operation | Target |
|-----------|--------|
| Well-formedness validation on INSERT | < 1 ms for 100 KB document |
| XMLTABLE on 10 KB doc, 100 rows | < 5 ms |

No benchmarks required — correctness over performance at this phase.

## Dependencies

- **Depends on:** Phase 20.18 (Composite types) — same codec pattern
- **New crate:** `roxmltree = "0.20"` added to `[workspace.dependencies]` in root
  `Cargo.toml` and to `axiomdb-types/Cargo.toml` (for validation) and
  `axiomdb-sql/Cargo.toml` (for XMLTABLE parsing + XPath evaluation)
- **Blocks:** XMLTABLE-dependent specs in future phases

## Open questions

All resolved:

1. **Crate choice:** `roxmltree` (pure Rust, zero-copy DOM, no C FFI) ✓
2. **XPath evaluator:** write internal minimal evaluator (Step 3); not a full XPath 1.0 crate ✓
3. **Fragment vs document:** accept both well-formed documents AND fragments ✓
4. **`XMLTYPE` keyword:** accepted as alias for `XML` ✓
5. **Wire display_len:** 16777215 (MEDIUMBLOB range) to allow large documents ✓

## Done criteria (overall)

- [ ] Steps 1, 2, 3 done criteria all satisfied
- [ ] `CREATE TABLE`, INSERT, SELECT XML column roundtrip
- [ ] XMLELEMENT + XMLFOREST work in SELECT list
- [ ] XMLTABLE shreds a multi-row document correctly
- [ ] `cargo nextest run --workspace` 100% pass
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke: XML column roundtrip + XMLELEMENT + XMLTABLE over MySQL protocol

## References

- `research/postgres/src/backend/utils/adt/xml.c` — PostgreSQL XMLTABLE/xmlelement/xmlforest impl (5167 lines, uses libxml2)
- `research/mariadb-server/plugin/type_xmltype/` — MariaDB XMLType as BLOB subtype
- `crates/axiomdb-sql/src/json_table.rs` — JSON_TABLE pattern to mirror for XMLTABLE
- `specs/fase-20/spec-20.19-ltree.md` — codec + ColumnType=N pattern reference
- SQL/XML standard: ISO/IEC 9075-14:2016 (SQL/XML)
