# Plan: 20.19 — ltree hierarchical path type

Phase: 20 — Types + import/export
Task: 20.19
Spec: specs/fase-20/spec-20.19-ltree.md
Status: in-progress

## Summary

Five steps: (1) `Value::Ltree` + `DataType::Ltree` + `ColumnType::Ltree=17` + codec +
validation logic in a new `ltree.rs` module; (2) parser recognizes `LTREE` column type,
`column_data_types` and DDL-to-ColumnType mapping updated, wire protocol wired;
(3) operators `@>`, `<@`, `~`, `||` dispatched from `eval_binary_op` via a new
`eval_binary_ltree` function + `~` dispatch before the regex arm; (4) functions
`nlevel`, `subpath`, `subltree`, `index`, `lca`, `text2ltree`, `ltree2text` in a
new `eval/functions/ltree.rs`; (5) 15+ integration tests + 4 wire assertions + closing protocol.

This order ensures each commit compiles: types first, then DDL, then operators, then functions,
then tests. No catalog heap is needed (ltree is stored as a validated string, not a named type).

## Dependencies

Must be done first:
- [x] spec-20.19-ltree.md approved
- [x] Phase 20.18 complete (ColumnType::Composite = 16 assigned)

Blocks:
- nothing in Phase 20

## Affected files

New files:
- `crates/axiomdb-types/src/ltree.rs` — validation + pure operations
- `crates/axiomdb-sql/src/eval/functions/ltree.rs` — function implementations
- `crates/axiomdb-sql/tests/integration_ltree.rs` — 15+ integration tests

Modified files:
- `crates/axiomdb-types/src/value.rs` — add `Value::Ltree(String)`
- `crates/axiomdb-types/src/types.rs` — add `DataType::Ltree`
- `crates/axiomdb-types/src/lib.rs` — `pub mod ltree; pub use ltree::validate_ltree_path;`
- `crates/axiomdb-types/src/codec.rs` — encode/decode Ltree
- `crates/axiomdb-types/src/coerce_api.rs` — Ltree identity + Text→Ltree cast
- `crates/axiomdb-types/src/coerce_helpers.rs` — `value_matches_type` Ltree arm
- `crates/axiomdb-types/src/array_codec.rs` — `ColumnType::Ltree = 17`
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Ltree = 17`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `parse_data_type` + `datatype_to_column_type`
- `crates/axiomdb-sql/src/table.rs` — `column_data_types` Ltree arm
- `crates/axiomdb-sql/src/eval/ops.rs` — ltree dispatch block + `eval_binary_ltree` fn
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — route ltree function names
- `crates/axiomdb-network/src/mysql/result.rs` — wire type + charset for Ltree
- `tools/wire-test.py` — `[20.19 ltree]` assertions

---

## Step 1 — Value::Ltree + DataType::Ltree + codec + validation

**Goal:** `Value::Ltree(String)` round-trips through `encode_row`/`decode_row`; path validation rejects malformed inputs.
**Files:** `axiomdb-types/src/{value.rs, types.rs, ltree.rs, lib.rs, codec.rs, coerce_api.rs, coerce_helpers.rs, array_codec.rs}`, `axiomdb-catalog/src/schema_database.rs`

### Implementation outline

```rust
// crates/axiomdb-types/src/ltree.rs  (NEW)

use axiomdb_core::DbError;

/// Validate that `s` is a well-formed ltree path.
/// Labels: [A-Za-z0-9_]+, separated by '.', at least 1 label,
/// max label length 255, max total 65535 bytes.
pub fn validate_ltree_path(s: &str) -> Result<(), DbError> {
    if s.is_empty() {
        return Err(DbError::InvalidValue { reason: "ltree path cannot be empty".into() });
    }
    if s.len() > 65535 {
        return Err(DbError::InvalidValue { reason: "ltree path too long (max 65535 bytes)".into() });
    }
    for label in s.split('.') {
        if label.is_empty() {
            return Err(DbError::InvalidValue { reason: "ltree path contains empty label (consecutive or leading/trailing dots)".into() });
        }
        if label.len() > 255 {
            return Err(DbError::InvalidValue { reason: format!("ltree label '{label}' exceeds 255 bytes") });
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(DbError::InvalidValue { reason: format!("ltree label '{label}' contains invalid characters (only [A-Za-z0-9_] allowed)") });
        }
    }
    Ok(())
}

/// Return true if `ancestor` is a prefix of `path` (at label boundaries).
/// Both must be valid ltree paths. Equal paths are ancestors of each other.
pub fn ltree_is_ancestor(ancestor: &str, path: &str) -> bool {
    if ancestor == path {
        return true;
    }
    path.starts_with(ancestor) && path.as_bytes().get(ancestor.len()) == Some(&b'.')
}

/// Concatenate two ltree paths: `left || right` → `left.right`.
pub fn ltree_concat(left: &str, right: &str) -> String {
    format!("{left}.{right}")
}

/// Match `path` against an lquery pattern.
/// Pattern labels: exact label or `*` (matches 0 or more labels).
/// The entire path must match the entire pattern (anchored).
pub fn lquery_match(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('.').collect();
    let pat_parts: Vec<&str> = pattern.split('.').collect();
    lquery_match_parts(&path_parts, &pat_parts)
}

fn lquery_match_parts(path: &[&str], pat: &[&str]) -> bool {
    match (path, pat) {
        ([], []) => true,
        (_, []) => false,
        ([], [p, rest @ ..]) => *p == "*" && lquery_match_parts(&[], rest),
        ([ph, pr @ ..], [p, pr2 @ ..]) => {
            if *p == "*" {
                // * matches 0 labels
                lquery_match_parts(path, pr2) ||
                // * matches 1+ labels
                lquery_match_parts(pr, pat)
            } else {
                *ph == *p && lquery_match_parts(pr, pr2)
            }
        }
    }
}

/// Return number of labels in `path`.
pub fn ltree_nlevel(path: &str) -> usize {
    path.split('.').count()
}

/// Return a sub-path from `offset` (0-based) with optional `len`.
/// Returns None if offset >= nlevel.
pub fn ltree_subpath(path: &str, offset: usize, len: Option<usize>) -> Option<String> {
    let labels: Vec<&str> = path.split('.').collect();
    if offset >= labels.len() {
        return None;
    }
    let end = len.map(|l| (offset + l).min(labels.len())).unwrap_or(labels.len());
    Some(labels[offset..end].join("."))
}

/// Return position (0-based) of `subpath` as consecutive labels within `path`,
/// starting search at `offset`. Returns None if not found.
pub fn ltree_index(path: &str, subpath: &str, offset: usize) -> Option<usize> {
    let path_labels: Vec<&str> = path.split('.').collect();
    let sub_labels: Vec<&str> = subpath.split('.').collect();
    let n = path_labels.len();
    let m = sub_labels.len();
    if m > n || offset + m > n {
        return None;
    }
    for i in offset..=(n - m) {
        if path_labels[i..i + m] == sub_labels[..] {
            return Some(i);
        }
    }
    None
}

/// Return the longest common ancestor of all paths.
/// Returns empty string if there is no common prefix.
pub fn ltree_lca(paths: &[&str]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let first_labels: Vec<&str> = paths[0].split('.').collect();
    let mut common_len = first_labels.len();
    for path in &paths[1..] {
        let labels: Vec<&str> = path.split('.').collect();
        common_len = common_len.min(labels.len());
        for (i, (a, b)) in first_labels[..common_len].iter().zip(labels.iter()).enumerate() {
            if a != b {
                common_len = i;
                break;
            }
        }
    }
    first_labels[..common_len].join(".")
}
```

```rust
// crates/axiomdb-types/src/value.rs — add variant at end of Value enum
    /// SQL ltree — hierarchical label path (Phase 20.19).
    /// Stored as a validated dot-separated ASCII string, e.g. `"a.b.c"`.
    Ltree(String),

// Display impl: just the string
Value::Ltree(s) => write!(f, "{s}"),
// variant_name:
Self::Ltree(_) => "Ltree",
```

```rust
// crates/axiomdb-types/src/types.rs — add variant
    /// SQL ltree — hierarchical label path (Phase 20.19).
    Ltree,

// display_name:
Self::Ltree => "LTREE".into(),
```

```rust
// crates/axiomdb-types/src/codec.rs — in encode_row match
Value::Ltree(s) => {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

// in decode_row match
DataType::Ltree => {
    let len = u32::from_le_bytes([bytes[pos], bytes[pos+1], bytes[pos+2], bytes[pos+3]]) as usize;
    pos += 4;
    let s = std::str::from_utf8(&bytes[pos..pos+len])
        .map_err(|_| DbError::InvalidValue { reason: "invalid UTF-8 in ltree column".into() })?
        .to_string();
    pos += len;
    values.push(Value::Ltree(s));
}

// encoded_len estimate:
DataType::Ltree => 4 + 32, // 4-byte prefix + avg path length
```

```rust
// crates/axiomdb-types/src/coerce_helpers.rs — value_matches_type
| (Value::Ltree(_), DataType::Ltree)

// crates/axiomdb-types/src/coerce_api.rs — identity + Text→Ltree
(v @ Value::Ltree(_), DataType::Ltree) => Ok(v),
(Value::Text(s), DataType::Ltree) => {
    validate_ltree_path(&s)?;
    Ok(Value::Ltree(s))
}
(Value::Ltree(s), DataType::Text) => Ok(Value::Text(s)),
```

```rust
// crates/axiomdb-catalog/src/schema_database.rs
Ltree = 17,   // SQL ltree hierarchical path (Phase 20.19)

// TryFrom<u8>:
17 => Ok(Self::Ltree),

// crates/axiomdb-types/src/array_codec.rs
Ltree = 17,
// TryFrom<u8>: 17 => Ok(Self::Ltree),
```

### Test to add

```rust
// In crates/axiomdb-types/src/ltree.rs (unit tests at bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn valid_single_label() { assert!(validate_ltree_path("a").is_ok()); }
    #[test] fn valid_multi_label()  { assert!(validate_ltree_path("a.b.c").is_ok()); }
    #[test] fn reject_empty()       { assert!(validate_ltree_path("").is_err()); }
    #[test] fn reject_leading_dot() { assert!(validate_ltree_path(".a").is_err()); }
    #[test] fn reject_trailing_dot(){ assert!(validate_ltree_path("a.").is_err()); }
    #[test] fn reject_consec_dot()  { assert!(validate_ltree_path("a..b").is_err()); }
    #[test] fn reject_space()       { assert!(validate_ltree_path("a b").is_err()); }
    #[test] fn reject_bang()        { assert!(validate_ltree_path("a!b").is_err()); }
    #[test] fn ancestor_equal()     { assert!(ltree_is_ancestor("a.b", "a.b")); }
    #[test] fn ancestor_prefix()    { assert!(ltree_is_ancestor("a.b", "a.b.c")); }
    #[test] fn not_ancestor_short() { assert!(!ltree_is_ancestor("a.b.c", "a.b")); }
    #[test] fn not_ancestor_label() { assert!(!ltree_is_ancestor("a.b", "a.bc")); }
    #[test] fn lquery_star()        { assert!(lquery_match("a.b.c", "*")); }
    #[test] fn lquery_prefix_star() { assert!(lquery_match("a.b.c", "a.*")); }
    #[test] fn lquery_mid_star()    { assert!(lquery_match("a.b.c", "*.b.*")); }
    #[test] fn lquery_exact()       { assert!(lquery_match("a.b", "a.b")); }
    #[test] fn lquery_no_match()    { assert!(!lquery_match("a.b.c", "a.b")); }
    #[test] fn nlevel_3()           { assert_eq!(ltree_nlevel("a.b.c"), 3); }
    #[test] fn subpath_offset()     { assert_eq!(ltree_subpath("a.b.c.d", 1, None), Some("b.c.d".into())); }
    #[test] fn subpath_len()        { assert_eq!(ltree_subpath("a.b.c.d", 1, Some(2)), Some("b.c".into())); }
    #[test] fn subpath_oob()        { assert!(ltree_subpath("a.b", 5, None).is_none()); }
    #[test] fn index_found()        { assert_eq!(ltree_index("a.b.c.a.b", "a.b", 0), Some(0)); }
    #[test] fn index_offset()       { assert_eq!(ltree_index("a.b.c.a.b", "a.b", 1), Some(3)); }
    #[test] fn index_not_found()    { assert_eq!(ltree_index("a.b.c", "x.y", 0), None); }
    #[test] fn lca_common()         { assert_eq!(ltree_lca(&["a.b.c", "a.b.d"]), "a.b"); }
    #[test] fn lca_disjoint()       { assert_eq!(ltree_lca(&["a.b", "c.d"]), ""); }
    #[test] fn lca_identical()      { assert_eq!(ltree_lca(&["a.b", "a.b"]), "a.b"); }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-types
./tools/vm.sh test -p axiomdb-catalog
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.19 step 1 — Value::Ltree + DataType::Ltree + codec + validation
```

---

## Step 2 — Parser + column DDL + wire protocol

**Goal:** `CREATE TABLE t (path LTREE)` works end-to-end; CAST('a.b' AS LTREE) coerces correctly; wire sends text charset.
**Files:** `axiomdb-sql/src/parser/ddl.rs`, `axiomdb-sql/src/table.rs`, `axiomdb-network/src/mysql/result.rs`

### Implementation outline

```rust
// crates/axiomdb-sql/src/parser/ddl.rs — parse_data_type, after MONEY arm
Token::Ident(s) if s.eq_ignore_ascii_case("LTREE") => {
    p.advance();
    Ok(ParsedDataType { data_type: DataType::Ltree, type_len: None, is_char: false })
}

// datatype_to_column_type (used by CREATE TABLE DDL executor)
DataType::Ltree => Ok(ColumnType::Ltree),
```

```rust
// crates/axiomdb-sql/src/table.rs — column_data_types, after Composite arm
ColumnType::Ltree => DataType::Ltree,
```

```rust
// crates/axiomdb-network/src/mysql/result.rs
// datatype_to_mysql_type:
DataType::Ltree => 0xfd,  // VAR_STRING

// column_display_len:
DataType::Ltree => 65_535,

// build_column_def charset_id match — add Ltree to text arm:
| DataType::Ltree => results_collation.id,

// value_to_text:
Value::Ltree(s) => s.clone(),   // path itself
```

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_ltree.rs (start with DDL subset)
#[test]
fn test_create_table_with_ltree_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx("CREATE TABLE cats (id INT, path LTREE)", ...).unwrap();
    // table visible in catalog with LTREE column
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table_in_database("axiomdb", "public", "cats").unwrap().unwrap();
    let cols = reader.list_columns(table.id).unwrap();
    let path_col = cols.iter().find(|c| c.name == "path").unwrap();
    assert_eq!(path_col.col_type, ColumnType::Ltree);
}

#[test]
fn test_insert_and_select_ltree_column() {
    // INSERT 'a.b.c' as text literal (implicit coerce), SELECT returns Value::Ltree
    let result = common::run_ctx("SELECT path FROM cats", ...).unwrap();
    let rows = common::rows(result);
    assert_eq!(rows[0][0], Value::Ltree("a.b.c".into()));
}

#[test]
fn test_cast_text_to_ltree_valid() { ... }
#[test]
fn test_cast_text_to_ltree_invalid() { // 'a..b' returns InvalidValue }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_ltree
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.19 step 2 — parser LTREE column type + wire protocol
```

---

## Step 3 — Operators: @>, <@, ~, ||

**Goal:** All 4 ltree binary operators produce correct results; `~` dispatches to lquery before regex path; `@>` / `<@` / `||` dispatch via `eval_binary_ltree`.
**Files:** `axiomdb-sql/src/eval/ops.rs`

### Implementation outline

```rust
// crates/axiomdb-sql/src/eval/ops.rs

// ── Ltree operator dispatch (Phase 20.19) ────────────────────────────────────
// Insert AFTER money dispatch block, BEFORE NULL propagation.
if matches!(&l, Value::Ltree(_))
    || (matches!(&r, Value::Ltree(_)) && matches!(op, BinaryOp::JsonContainedBy))
{
    return eval_binary_ltree(op, l, r);
}

// In the main BinaryOp match (RegexpTilde arm), add ltree dispatch:
BinaryOp::RegexpTilde => {
    if matches!(&l, Value::Ltree(_)) {
        // lquery pattern match (Phase 20.19)
        let path = match &l { Value::Ltree(s) => s.as_str(), _ => unreachable!() };
        let pattern = match &r { Value::Text(s) => s.as_str(), Value::Ltree(s) => s.as_str(),
            _ => return Err(DbError::InvalidValue { reason: "lquery pattern must be TEXT".into() }) };
        Ok(Value::Bool(axiomdb_types::ltree::lquery_match(path, pattern)))
    } else {
        eval_regexp_tilde(l, r, false, false)
    }
}

// New function at bottom of ops.rs:
fn eval_binary_ltree(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // NULL propagation
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    use axiomdb_types::ltree::{ltree_is_ancestor, ltree_concat};
    match (op, l, r) {
        // @> ancestor-or-equal
        (BinaryOp::JsonContains, Value::Ltree(ref lpath), Value::Ltree(ref rpath)) =>
            Ok(Value::Bool(ltree_is_ancestor(lpath, rpath))),
        // <@ descendant-or-equal (reverse)
        (BinaryOp::JsonContainedBy, Value::Ltree(ref lpath), Value::Ltree(ref rpath)) =>
            Ok(Value::Bool(ltree_is_ancestor(rpath, lpath))),
        // || concatenation
        (BinaryOp::Concat, Value::Ltree(ref l), Value::Ltree(ref r)) =>
            Ok(Value::Ltree(ltree_concat(l, r))),
        _ => Err(DbError::InvalidValue {
            reason: format!("operator {op:?} not supported for ltree operands"),
        }),
    }
}
```

### Test to add

```rust
// integration_ltree.rs
#[test] fn test_ancestor_op() { /* 'a.b' @> 'a.b.c' → 1 row */ }
#[test] fn test_descendant_op() { /* 'a.b.c' <@ 'a.b' → 1 row */ }
#[test] fn test_lquery_match_star() { /* path ~ '*.phones.*' */ }
#[test] fn test_lquery_no_match() { /* path ~ 'nonexistent' → 0 rows */ }
#[test] fn test_concat_op() { /* SELECT 'a.b'::LTREE || 'c'::LTREE → 'a.b.c' */ }
#[test] fn test_ancestor_equal_paths() { /* 'a.b' @> 'a.b' → true */ }
#[test] fn test_null_propagation_op() { /* NULL @> 'a.b' → NULL */ }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_ltree
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.19 step 3 — ltree operators @>, <@, ~, ||
```

---

## Step 4 — Functions: nlevel, subpath, subltree, index, lca, text2ltree, ltree2text

**Goal:** All 7 ltree scalar functions work correctly in SELECT, WHERE, and expressions.
**Files:** `axiomdb-sql/src/eval/functions/ltree.rs` (new), `axiomdb-sql/src/eval/functions/mod.rs`

### Implementation outline

```rust
// crates/axiomdb-sql/src/eval/functions/ltree.rs  (NEW)
use axiomdb_core::DbError;
use axiomdb_types::Value;
use axiomdb_types::ltree::*;
use crate::expr::Expr;
use super::eval_expr;

pub(crate) fn eval_ltree_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
) -> Result<Value, DbError> {
    match name {
        "nlevel" => {
            require_arity("nlevel", args, 1..=1)?;
            match eval_expr(&args[0], row)? {
                Value::Null => Ok(Value::Null),
                Value::Ltree(s) => Ok(Value::Int(ltree_nlevel(&s) as i32)),
                other => Err(arg_type_err("nlevel", "LTREE", &other)),
            }
        }
        "subpath" => {
            require_arity("subpath", args, 2..=3)?;
            let path = eval_expr(&args[0], row)?;
            let offset = eval_expr(&args[1], row)?;
            let len = if args.len() == 3 { Some(eval_expr(&args[2], row)?) } else { None };
            match (path, offset) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Ltree(s), Value::Int(off)) => {
                    if off < 0 {
                        return Err(DbError::InvalidValue { reason: "subpath offset cannot be negative".into() });
                    }
                    let len_val = match len {
                        None => None,
                        Some(Value::Int(l)) if l >= 0 => Some(l as usize),
                        Some(Value::Null) => return Ok(Value::Null),
                        _ => return Err(DbError::InvalidValue { reason: "subpath len must be non-negative INT".into() }),
                    };
                    ltree_subpath(&s, off as usize, len_val)
                        .map(Value::Ltree)
                        .ok_or_else(|| DbError::InvalidValue { reason: format!("subpath offset {off} out of range for path '{s}'") })
                }
                _ => Err(DbError::InvalidValue { reason: "subpath requires LTREE and INT arguments".into() }),
            }
        }
        "subltree" => {
            // subltree(path, start, end) = subpath(path, start, end - start)
            require_arity("subltree", args, 3..=3)?;
            let path = eval_expr(&args[0], row)?;
            let start = eval_expr(&args[1], row)?;
            let end = eval_expr(&args[2], row)?;
            match (path, start, end) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Ltree(s), Value::Int(start), Value::Int(end)) => {
                    let len = (end - start).max(0) as usize;
                    ltree_subpath(&s, start as usize, Some(len))
                        .map(Value::Ltree)
                        .ok_or_else(|| DbError::InvalidValue { reason: "subltree range out of bounds".into() })
                }
                _ => Err(DbError::InvalidValue { reason: "subltree requires LTREE, INT, INT".into() }),
            }
        }
        "index" => {
            require_arity("index", args, 2..=3)?;
            let path = eval_expr(&args[0], row)?;
            let subpath = eval_expr(&args[1], row)?;
            let offset = if args.len() == 3 { eval_expr(&args[2], row)? } else { Value::Int(0) };
            match (path, subpath, offset) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Ltree(p), Value::Ltree(sub), Value::Int(off)) => {
                    if off < 0 {
                        return Err(DbError::InvalidValue { reason: "index offset cannot be negative".into() });
                    }
                    let result = ltree_index(&p, &sub, off as usize)
                        .map(|i| i as i32)
                        .unwrap_or(-1);
                    Ok(Value::Int(result))
                }
                _ => Err(DbError::InvalidValue { reason: "index requires LTREE, LTREE [, INT]".into() }),
            }
        }
        "lca" => {
            if args.is_empty() {
                return Err(DbError::InvalidValue { reason: "lca requires at least 1 argument".into() });
            }
            let mut paths = Vec::with_capacity(args.len());
            for arg in args {
                match eval_expr(arg, row)? {
                    Value::Null => return Ok(Value::Null),
                    Value::Ltree(s) => paths.push(s),
                    other => return Err(arg_type_err("lca", "LTREE", &other)),
                }
            }
            let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
            Ok(Value::Ltree(ltree_lca(&refs)))
        }
        "text2ltree" => {
            require_arity("text2ltree", args, 1..=1)?;
            match eval_expr(&args[0], row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) | Value::Ltree(s) => {
                    axiomdb_types::ltree::validate_ltree_path(&s)?;
                    Ok(Value::Ltree(s))
                }
                other => Err(arg_type_err("text2ltree", "TEXT", &other)),
            }
        }
        "ltree2text" => {
            require_arity("ltree2text", args, 1..=1)?;
            match eval_expr(&args[0], row)? {
                Value::Null => Ok(Value::Null),
                Value::Ltree(s) => Ok(Value::Text(s)),
                other => Err(arg_type_err("ltree2text", "LTREE", &other)),
            }
        }
        _ => unreachable!("unknown ltree function: {name}"),
    }
}
```

```rust
// crates/axiomdb-sql/src/eval/functions/mod.rs — add routing before or after range block:
"nlevel" | "subpath" | "subltree" | "index" | "lca" | "text2ltree" | "ltree2text" => {
    super::ltree::eval_ltree_function(name, args, row)
}
```

Also add `mod ltree;` to `functions/mod.rs` or equivalent.

### Test to add

```rust
// integration_ltree.rs
#[test] fn test_nlevel() { /* SELECT nlevel('a.b.c') → 3 */ }
#[test] fn test_subpath_no_len() { /* SELECT subpath('a.b.c.d', 1) → 'b.c.d' */ }
#[test] fn test_subpath_with_len() { /* SELECT subpath('a.b.c.d', 1, 2) → 'b.c' */ }
#[test] fn test_subltree() { /* SELECT subltree('a.b.c.d', 0, 2) → 'a.b' */ }
#[test] fn test_index_found() { /* SELECT index('a.b.c.a.b'::LTREE, 'a.b'::LTREE) → 0 */ }
#[test] fn test_index_with_offset() { /* → 3 */ }
#[test] fn test_index_not_found() { /* → -1 */ }
#[test] fn test_lca_common() { /* SELECT lca('a.b.c'::LTREE, 'a.b.d'::LTREE) → 'a.b' */ }
#[test] fn test_lca_disjoint() { /* → '' (empty ltree) */ }
#[test] fn test_text2ltree_valid() { /* → Value::Ltree */ }
#[test] fn test_text2ltree_invalid() { /* → InvalidValue */ }
#[test] fn test_ltree2text() { /* → Value::Text */ }
#[test] fn test_null_propagation_nlevel() { /* nlevel(NULL) → NULL */ }
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_ltree
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.19 step 4 — ltree functions nlevel/subpath/index/lca/text2ltree/ltree2text
```

---

## Step 5 — Integration tests + wire smoke + close

**Goal:** All 15+ integration tests pass; 4+ wire assertions green; full workspace clean; docs updated.
**Files:** `crates/axiomdb-sql/tests/integration_ltree.rs` (complete), `tools/wire-test.py`, `docs/progreso.md`, `docs/fase-20.md`, `docs-site/`

### Integration tests (complete list)

Tests added across steps 2–4 plus closing coverage:

```
test_create_table_with_ltree_column
test_insert_and_select_ltree_value
test_cast_text_to_ltree_valid
test_cast_text_to_ltree_invalid_double_dot
test_cast_text_to_ltree_invalid_empty
test_cast_text_to_ltree_invalid_bad_char
test_ancestor_op
test_descendant_op
test_lquery_match_star
test_lquery_exact_match
test_lquery_no_match
test_concat_op
test_null_propagation_ancestor
test_nlevel
test_subpath_no_len
test_subpath_with_len
test_subltree
test_index_found
test_index_with_offset
test_index_not_found
test_lca_common_ancestor
test_lca_disjoint_paths
test_text2ltree_valid
test_text2ltree_invalid
test_ltree2text
```

### Wire test additions

```python
# ── Phase 20.19 — ltree ───────────────────────────────────────────────────────
print("\n[20.19 ltree]")
cur.execute("CREATE TABLE ltree_test (id INT PRIMARY KEY, path LTREE)")
conn.commit()
cur.execute("INSERT INTO ltree_test VALUES (1, 'electronics.phones'), (2, 'electronics.phones.smartphones'), (3, 'electronics.laptops')")
conn.commit()

cur.execute("SELECT path FROM ltree_test WHERE 'electronics' @> path ORDER BY id")
rows = cur.fetchall()
ok("[20.19 ltree] @> ancestor returns all paths under electronics",
   len(rows) == 3, rows)

cur.execute("SELECT path FROM ltree_test WHERE path <@ 'electronics.phones'")
rows = cur.fetchall()
ok("[20.19 ltree] <@ descendant returns correct paths",
   len(rows) == 2, rows)

cur.execute("SELECT path FROM ltree_test WHERE path ~ 'electronics.*.smartphones'")
rows = cur.fetchall()
ok("[20.19 ltree] ~ lquery pattern matches smartphones path",
   len(rows) == 1 and rows[0][0] == 'electronics.phones.smartphones', rows)

cur.execute("SELECT nlevel('electronics.phones.smartphones'::LTREE)")
row = cur.fetchone()
ok("[20.19 ltree] nlevel returns 3",
   row[0] == 3, row)

cur.execute("DROP TABLE ltree_test")
conn.commit()
```

### Closing protocol

```bash
./tools/vm.sh test --workspace          # 4200+ pass
./tools/vm.sh clippy                    # clean
./tools/vm.sh fmt-check                 # clean
./tools/vm.sh wire                      # [20.19 ltree] 4/4 green
```

Update:
- `docs/progreso.md` — 20.19 marked `[x] ✅`
- `docs/fase-20.md` — 20.19 section added
- `docs-site/src/user-guide/sql-reference/ddl.md` — LTREE column type
- `docs-site/src/user-guide/sql-reference/expressions.md` — ltree operators + functions
- `docs-site/src/internals/sql-parser.md` — ltree section

### Verification against spec

- [x] `Value::Ltree`, `DataType::Ltree`, `ColumnType::Ltree=17` exist
- [x] `validate_ltree_path` rejects all invalid inputs in spec
- [x] `encode_row`/`decode_row` round-trip Ltree
- [x] `CREATE TABLE + INSERT` works
- [x] `CAST` validates on write
- [x] All 4 operators correct
- [x] All 7 functions correct
- [x] NULL propagation everywhere
- [x] ≥ 15 integration tests
- [x] ≥ 4 wire assertions
- [x] Workspace clean

### Final commit

```
feat(fase-20): complete 20.19 ltree hierarchical path type

Implements specs/fase-20/spec-20.19-ltree.md
Plan: specs/fase-20/plan-20.19-ltree.md
Tests: 25 new tests (unit + integration)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `~` dispatch conflict with regex (RegexpTilde) | low | check `Value::Ltree` first in the arm |
| `@>` / `<@` conflict with JSONB path (JsonContains) | low | dispatch before NULL propagation, guard on `Value::Ltree` |
| `lquery_match` stack overflow on deep recursion | low | max path depth is bounded by 65535-byte limit (~thousands of labels) — acceptable |
| `lca` with 0 args | low | explicit arity check returns `InvalidValue` |

## Rollback plan

1. `git reset --hard <commit before step 1>`
2. Mark spec status → `draft` with note

## Estimated effort

Total: ~3 hours
Per step: step 1: 45min, step 2: 30min, step 3: 30min, step 4: 45min, step 5: 30min
