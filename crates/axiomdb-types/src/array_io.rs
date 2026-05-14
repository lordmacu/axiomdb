//! PostgreSQL-compatible array text I/O — `array_to_text` and `text_to_array`.
//!
//! ## Text format
//!
//! ```text
//! {}                          ← empty array
//! {1,2,3}                     ← 1D int array
//! {{1,2},{3,4}}              ← 2D int array (2×2)
//! {foo,bar,baz}              ← 1D text array (no quoting needed)
//! {"hello, world",barelem}   ← elements with commas/spaces are double-quoted
//! {"say \"hi\"",normal}      ← double-quotes inside are escaped with backslash
//! {1,NULL,3}                 ← NULL elements written as NULL (unquoted)
//! {"NULL","not null"}        ← string "NULL" must be quoted to avoid ambiguity
//! [1:3]={1,2,3}             ← explicit lower bounds (only if any lbound != 1)
//! [-1:1][1:3]={{0,1,2},{3,4,5}}  ← multidimensional explicit bounds
//! ```
//!
//! ## Quoting rules
//!
//! - NULL elements → literal `NULL` (unquoted)
//! - Elements with `{`, `}`, `,`, `"`, `\` or whitespace → double-quoted + backslash-escaped
//! - String "NULL" must be quoted to avoid ambiguity with NULL literal

use axiomdb_core::error::DbError;

use crate::array_codec::ColumnType;
use crate::Value;

use crate::array_codec::{decode_array, encode_array, encode_array_nd};

/// Converts an array blob to a PG-compatible text string.
///
/// `array_to_text` takes only blob bytes and the schema's DataType,
/// and uses `decode_array` to reconstruct the element values, then
/// formats them according to PG quoting rules.
///
/// The blob must be a valid array encoding (as produced by `encode_array`).
pub fn array_to_text(blob: &[u8]) -> Result<String, DbError> {
    let (value, _elem_type, ndim) = decode_array(blob)?;

    let Value::Array(elems) = value else {
        return Err(DbError::InvalidValue {
            reason: "blob is not a valid array encoding".to_string(),
        });
    };

    // For empty array
    if ndim == 0 {
        return Ok("{}".to_string());
    }

    // We need dims from the blob to determine 2D formatting.
    // We stored dims at offset 13 (after header: total_len+ndim+dataoffset+elemtype).
    // Re-decode just to get dims (this is a bit redundant but decode_array already parses it).
    // Alternative: parse the blob header directly for dims.
    // Let's re-read the dims from the blob without full decode.
    let dims = read_dims_from_blob(blob)?;

    format_array_elements(&elems, &dims, ndim as usize)
}

/// Reads dimension lengths from an array blob header.
fn read_dims_from_blob(blob: &[u8]) -> Result<Vec<i32>, DbError> {
    if blob.len() < 13 {
        return Err(DbError::ParseError {
            message: "blob too short for dims".to_string(),
            position: None,
        });
    }
    let ndim = i32::from_le_bytes(blob[4..8].try_into().map_err(|_| DbError::ParseError {
        message: "internal: fixed-size slice conversion".into(),
        position: None,
    })?) as usize;
    if ndim == 0 {
        return Ok(vec![]);
    }
    let header_size = 13 + ndim * 8;
    if blob.len() < header_size {
        return Err(DbError::ParseError {
            message: "blob too short for dimension data".to_string(),
            position: None,
        });
    }
    let mut dims = Vec::with_capacity(ndim);
    for i in 0..ndim {
        let d =
            i32::from_le_bytes(blob[13 + i * 4..13 + (i + 1) * 4].try_into().map_err(|_| {
                DbError::ParseError {
                    message: "internal: fixed-size slice conversion reading dim".into(),
                    position: None,
                }
            })?);
        dims.push(d);
    }
    Ok(dims)
}

/// Formats array elements as PG text, given dimensions and ndims.
fn format_array_elements(
    elems: &[crate::value::Value],
    dims: &[i32],
    ndim: usize,
) -> Result<String, DbError> {
    if dims.is_empty() {
        return Ok("{}".to_string());
    }

    let nitems: usize = dims.iter().map(|&d| d as usize).product();
    if elems.len() != nitems {
        return Err(DbError::InvalidValue {
            reason: format!(
                "element count ({}) does not match dimensions {:?}",
                elems.len(),
                dims
            ),
        });
    }

    let mut result = String::new();

    if ndim == 1 {
        // Simple 1D: {elem1,elem2,...}
        result.push('{');
        for (i, elem) in elems.iter().enumerate() {
            if i > 0 {
                result.push(',');
            }
            result.push_str(&format_element_text(elem));
        }
        result.push('}');
    } else {
        // Multidimensional: recursively format slices
        // format_nd_slice handles all brace wrapping
        format_nd_slice(&mut result, elems, dims, 0, 0);
    }

    Ok(result)
}

/// Recursively formats an n-dimensional slice as PG text.
fn format_nd_slice(
    out: &mut String,
    elems: &[crate::value::Value],
    dims: &[i32],
    dim_idx: usize,
    offset: usize,
) {
    let stride: usize = dims[dim_idx + 1..].iter().map(|&d| d as usize).product();

    out.push('{');
    for i in 0..(dims[dim_idx] as usize) {
        if i > 0 {
            out.push(',');
        }
        let elem_offset = offset + i * stride;
        if dim_idx == dims.len() - 1 {
            // innermost dimension — format actual elements
            for j in 0..stride {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&format_element_text(&elems[elem_offset + j]));
            }
        } else {
            // another dimension level — recurse
            format_nd_slice(out, elems, dims, dim_idx + 1, elem_offset);
        }
    }
    out.push('}');
}

/// Formats a single element value as PG-compatible text.
///
/// - NULL → "NULL" (unquoted)
/// - Strings needing quoting are double-quoted with backslash-escaped special chars
fn format_element_text(elem: &crate::value::Value) -> String {
    match elem {
        crate::value::Value::Null => "NULL".to_string(),
        crate::value::Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        crate::value::Value::Int(n) => n.to_string(),
        crate::value::Value::BigInt(n) => n.to_string(),
        crate::value::Value::Real(f) => {
            if f.is_infinite() {
                if f.is_sign_positive() {
                    "Infinity"
                } else {
                    "-Infinity"
                }
                .to_string()
            } else {
                format!("{}", f)
            }
        }
        crate::value::Value::Decimal(m, s) => format!("{}e-{}", m, s),
        crate::value::Value::Text(s) => quote_text_element(s),
        crate::value::Value::Bytes(b) => {
            // PG encodes bytea as hex: \\x...
            let hex: String = b.iter().map(|byte| format!("{:02x}", byte)).collect();
            format!("\\\\x{}", hex)
        }
        crate::value::Value::Date(d) => format!("{}", d),
        crate::value::Value::Timestamp(t) => format!("{}", t),
        crate::value::Value::Uuid(u) => {
            format!(
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
                u16::from_be_bytes([u[4], u[5]]),
                u16::from_be_bytes([u[6], u[7]]),
                u16::from_be_bytes([u[8], u[9]]),
                u64::from_be_bytes([0, 0, u[10], u[11], u[12], u[13], u[14], u[15]])
            )
        }
        crate::value::Value::Json(s) => quote_text_element(s),
        crate::value::Value::Jsonb(blob) => {
            // Decode jsonb to string
            match crate::jsonb::JsonbDecoder::to_string(blob) {
                Ok(s) => quote_text_element(&s),
                Err(_) => quote_text_element("<invalid jsonb>"),
            }
        }
        crate::value::Value::Array(_) => {
            // This shouldn't happen at leaf level
            "{}".to_string()
        }
    }
}

/// Quotes a text element according to PG array quoting rules.
///
/// An element needs quoting if it contains any of: `{`, `}`, `,`, `"`, `\`,
/// or whitespace (space, tab, newline, carriage return).
///
/// Double-quotes are escaped with backslash. Backslash is escaped with backslash.
fn quote_text_element(s: &str) -> String {
    let needs_quote = s.bytes().any(|c| {
        matches!(
            c,
            b'{' | b'}' | b',' | b'"' | b'\\' | b' ' | b'\t' | b'\n' | b'\r'
        )
    });

    if !needs_quote && !s.eq_ignore_ascii_case("NULL") {
        // Don't quote simple strings that aren't "NULL"
        return s.to_string();
    }

    let mut result = String::with_capacity(s.len() + 4);
    result.push('"');
    for c in s.chars() {
        match c {
            '"' => {
                result.push('\\');
                result.push('"');
            }
            '\\' => {
                result.push('\\');
                result.push('\\');
            }
            c => {
                result.push(c);
            }
        }
    }
    result.push('"');
    result
}

/// Parses a PG text format array string and returns the array blob bytes.
///
/// `text_to_array` handles:
/// - `{elem1,elem2,...}` 1D format
/// - `{{a,b},{c,d}}` 2D format
/// - `{}` empty array
/// - `NULL` elements (unquoted)
/// - Quoted elements with backslash escaping
/// - Explicit bounds: `[1:3]={1,2,3}` (if any lbound != 1)
///
/// The element type is required to know how to parse and encode each element.
pub fn text_to_array(text: &str, elem_type: ColumnType) -> Result<Vec<u8>, DbError> {
    let text = text.trim();

    // Empty array
    if text == "{}" {
        let empty: crate::value::Value = crate::value::Value::Array(vec![]);
        return encode_array(&empty, elem_type);
    }

    // Check for explicit bounds prefix: [lb:ub]= or [-1:1]={...}
    let (text, dims, lbounds) = parse_explicit_bounds(text)?;

    // Parse the array literal
    let elements = parse_array_literal(text)?;

    // Validate dimensions match
    let ndim = dims.len() as i32;
    if ndim == 0 && !elements.is_empty() {
        return Err(DbError::ParseError {
            message: "empty array must have ndim=0".to_string(),
            position: None,
        });
    }

    if ndim > MAX_ARRAY_DIMS {
        return Err(DbError::InvalidValue {
            reason: format!(
                "number of array dimensions ({}) exceeds the maximum allowed ({})",
                ndim, MAX_ARRAY_DIMS
            ),
        });
    }

    let expected_count: usize = dims.iter().map(|&d| d as usize).product();
    if elements.len() != expected_count {
        return Err(DbError::InvalidValue {
            reason: format!(
                "multidimensional arrays must have array expressions with matching dimensions (got {} elements, expected {:?} = {})",
                elements.len(),
                dims,
                expected_count
            ),
        });
    }

    // Build Value::Array from parsed elements
    let mut values: Vec<crate::value::Value> = Vec::with_capacity(elements.len());
    // Track if we recursively parsed sub-arrays (indicating multidimensional array)
    let mut did_recursive_parse = false;
    // Track inner dims if we recursively parsed
    let mut inner_dims: Vec<i32> = Vec::new();

    for elem_text in &elements {
        let value = if ndim > 1 || (ndim == 1 && elem_text.trim().starts_with('{')) {
            // For ND arrays, or for ndim=1 where elements look like sub-arrays,
            // recursively parse sub-arrays and flatten
            did_recursive_parse = true;
            let sub_blob = text_to_array(elem_text, elem_type)?;
            let (sub_value, _, _) = crate::array_codec::decode_array(&sub_blob)?;
            sub_value
        } else {
            parse_element_text(elem_text, elem_type)?
        };
        // For ND, sub_value is Value::Array; we need to flatten it
        if let crate::value::Value::Array(sub_elems) = value {
            if inner_dims.is_empty() {
                inner_dims.push(sub_elems.len() as i32);
            }
            values.extend(sub_elems);
        } else {
            values.push(value);
        }
    }

    let array_value = crate::value::Value::Array(values);

    // If we recursively parsed sub-arrays (ndim was actually > 1), use encode_array_nd
    if did_recursive_parse && !inner_dims.is_empty() {
        // The actual ndim is 1 + inner_dims.len()
        let actual_ndim = (1 + inner_dims.len()) as i32;
        let mut actual_dims = vec![elements.len() as i32];
        actual_dims.extend(inner_dims);
        return encode_array_nd(&array_value, elem_type, actual_ndim, &actual_dims);
    }

    // If we had explicit bounds, use encode_array_nd
    if lbounds.iter().any(|&lb| lb != 1) || ndim == 0 {
        // encode_array handles ndim=1 case; for explicit bounds use encode_array_nd
        // But our encode_array_nd only supports default lbound=1 for now
        // For explicit bounds != 1, we'd need to modify encode_array_nd
        // Since PG mostly uses default bounds, we'll handle ndim==0 case and
        // fall back to encode_array for default bounds
        if ndim == 0 {
            return encode_array(&array_value, elem_type);
        }
        // With explicit bounds different from 1, we need special handling
        // For now, return error if lbound != 1 (not common in practice)
        return Err(DbError::InvalidValue {
            reason: "explicit array lower bounds other than 1 not yet implemented".to_string(),
        });
    }

    // Standard case: 1D with default bounds
    if ndim == 1 {
        return encode_array(&array_value, elem_type);
    }

    // Multidimensional with default bounds
    encode_array_nd(&array_value, elem_type, ndim, &dims)
}

/// Maximum array dimensions.
const MAX_ARRAY_DIMS: i32 = 6;

/// Parses explicit bounds from array text, e.g., `[1:3]={1,2,3}`.
///
/// Returns (remaining_text, dims, lbounds).
fn parse_explicit_bounds(text: &str) -> Result<(&str, Vec<i32>, Vec<i32>), DbError> {
    if !text.starts_with('[') {
        // No explicit bounds — infer from array structure
        return infer_dims_from_text(text);
    }

    // Parse [lb1:ub1][lb2:ub2]...={...}
    let mut dims = Vec::new();
    let mut lbounds = Vec::new();
    let mut remaining = text;

    while remaining.starts_with('[') {
        let end = remaining.find(']').ok_or_else(|| DbError::ParseError {
            message: "unterminated array bound bracket".to_string(),
            position: None,
        })?;
        let bracket = &remaining[1..end];
        remaining = &remaining[end + 1..];

        // Parse "lb:ub"
        let colon = bracket.find(':').ok_or_else(|| DbError::ParseError {
            message: "invalid array bound format, expected 'lb:ub'".to_string(),
            position: None,
        })?;
        let lb: i32 = bracket[..colon].parse().map_err(|_| DbError::ParseError {
            message: format!("invalid lower bound: {}", &bracket[..colon]),
            position: None,
        })?;
        let ub: i32 = bracket[colon + 1..]
            .parse()
            .map_err(|_| DbError::ParseError {
                message: format!("invalid upper bound: {}", &bracket[colon + 1..]),
                position: None,
            })?;
        if ub < lb {
            return Err(DbError::InvalidValue {
                reason: "array upper bound cannot be less than lower bound".to_string(),
            });
        }
        dims.push(ub - lb + 1);
        lbounds.push(lb);
    }

    // Skip '=' if present
    if remaining.starts_with('=') {
        remaining = &remaining[1..];
    }

    Ok((remaining, dims, lbounds))
}

/// Infers dimensions from the array text structure.
fn infer_dims_from_text(text: &str) -> Result<(&str, Vec<i32>, Vec<i32>), DbError> {
    // For PG array text, we parse to get top-level elements.
    // We need to figure out the dimensions without fully parsing.
    //
    // Strategy: count top-level elements by tracking depth.
    // A comma at depth=1 separates top-level elements.
    // A comma at depth>1 is inside a nested array (sub-array).
    let text = text.trim();
    if !text.starts_with('{') {
        return Err(DbError::ParseError {
            message: format!("expected '{{' at start of array literal, got: {}", text),
            position: None,
        });
    }

    // Count top-level elements
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let mut in_escape = false;
    let mut elem_count = 0;
    // Did we just close a sub-array (went from depth >= 2 to depth 1)?
    let mut just_closed_subarray = false;
    // Have we entered any nested structure (depth >= 2)?
    let mut entered_nested = false;

    for c in text.chars() {
        if in_escape {
            in_escape = false;
            continue;
        }
        match c {
            '\\' if in_quote => {
                in_escape = true;
            }
            '"' if !in_quote => {
                // Opening quote: enter quoted element
                in_quote = true;
                depth += 1;
            }
            '"' if in_quote => {
                // Closing quote: exit quoted element
                in_quote = false;
                depth -= 1;
            }
            '{' if !in_quote => {
                depth += 1;
                if depth >= 2 {
                    entered_nested = true;
                }
                just_closed_subarray = false;
            }
            '}' if !in_quote => {
                let prev_depth = depth;
                depth = depth.saturating_sub(1);
                // If we went from depth >= 2 to depth 1, we just closed a sub-array
                if prev_depth >= 2 && depth == 1 {
                    just_closed_subarray = true;
                    elem_count += 1; // Count the sub-array we just closed
                }
                // If we're closing the outermost array (depth 0):
                // - For flat arrays (never entered nested): we just finished the last element
                // - For nested arrays: don't increment (already counted sub-arrays)
                if depth == 0 && prev_depth == 1 && !entered_nested {
                    elem_count += 1;
                }
                // Reset flag only when we're closing the outermost AND we're not in a nested context
                // (i.e., we just closed the final element of a flat array)
                if depth == 0 && prev_depth == 1 && !entered_nested {
                    just_closed_subarray = false;
                }
            }
            ',' if depth == 1 && !in_quote => {
                // Comma between top-level elements
                // Skip if we just closed a sub-array (its closing brace is part of the element)
                if !just_closed_subarray {
                    elem_count += 1;
                }
                // Reset flag after handling it
                just_closed_subarray = false;
            }
            ',' if depth == 0 && !in_quote => {
                // Comma in flat array
                elem_count += 1;
            }
            _ => {}
        }
    }

    // If no top-level elements found (empty array check)
    if elem_count == 0 && text.contains("{}") {
        return Ok((text, vec![], vec![]));
    }

    Ok((text, vec![elem_count], vec![1]))
}

/// Parses the array literal body `{...}` into a flat list of element strings.
///
/// Handles nested braces for multidimensional arrays, quoted strings, and backslash escaping.
fn parse_array_literal(text: &str) -> Result<Vec<&str>, DbError> {
    let text = text.trim();
    if !text.starts_with('{') || !text.ends_with('}') {
        return Err(DbError::ParseError {
            message: format!("array literal must be enclosed in braces, got: {}", text),
            position: None,
        });
    }
    let inner = &text[1..text.len() - 1];
    // Determine initial depth based on whether content starts with nested sub-array
    // After stripping outer braces, if content starts with '{', we're in a nested array
    // and should start at depth 1 (already inside outer array)
    let initial_depth = if inner.starts_with('{') { 1 } else { 0 };
    parse_elements(inner, initial_depth)
}

/// Parses comma-separated elements from inside `{...}`.
///
/// Handles: quoted strings, NULL, nested braces for 2D, backslash escapes.
/// initial_depth: 0 for flat arrays, 1 for nested (after stripping outer braces of parent)
fn parse_elements(s: &str, initial_depth: i32) -> Result<Vec<&str>, DbError> {
    if s.is_empty() {
        return Ok(vec![]);
    }

    let mut elements = Vec::new();
    let mut depth = initial_depth;
    let mut in_quote = false;
    let mut elem_start = 0;
    let mut i = 0;
    // Did we just close a sub-array (depth went from 2+ to 1)?
    let mut just_closed_subarray = false;
    // Have we entered any nested structure (depth >= 2)?
    // If initial_depth > 0, we're already inside the outer array structure
    let mut entered_nested = initial_depth > 0;

    while i < s.len() {
        let c = s[i..].chars().next().ok_or_else(|| DbError::ParseError {
            message: "unexpected end of array literal string".into(),
            position: None,
        })?;
        match c {
            '"' if !in_quote => {
                // Opening quote: enter quoted element (increase depth)
                in_quote = true;
                depth += 1;
                i += c.len_utf8();
            }
            '"' if in_quote => {
                // Closing quote: exit quoted element (decrease depth)
                in_quote = false;
                depth -= 1;
                i += c.len_utf8();
            }
            '{' => {
                depth += 1;
                if depth >= 2 {
                    entered_nested = true;
                }
                just_closed_subarray = false;
                i += c.len_utf8();
            }
            '}' => {
                // Capture prev_depth BEFORE decrementing
                let prev_depth = depth;
                depth -= 1;
                // If we went from depth 2+ to depth 1, we just closed a sub-array
                if prev_depth >= 2 && depth == 1 {
                    // Push the sub-array element we just closed
                    if elem_start < i {
                        elements.push(&s[elem_start..=i]); // include the }
                        elem_start = i + 1;
                    }
                    just_closed_subarray = true;
                }
                // For flat arrays, closing outermost means we finished the last element
                if depth == 0 && prev_depth == 1 && !entered_nested {
                    // Push remaining element for flat arrays
                    if elem_start < i {
                        elements.push(&s[elem_start..i]);
                    }
                }
                // Reset flag only for flat arrays when closing outermost
                if depth == 0 && !entered_nested {
                    just_closed_subarray = false;
                }
                i += c.len_utf8();
            }
            ',' if depth == 1 && !in_quote => {
                // Top-level comma (separates sub-arrays)
                // Only push if we didn't just close a sub-array (its closing brace was already pushed)
                if !just_closed_subarray && elem_start < i {
                    elements.push(&s[elem_start..i]);
                }
                i += 1;
                elem_start = i;
                // Don't reset flag here - it stays true until we start a new sub-array
                // This prevents the end-of-loop check from pushing again
            }
            ',' if depth == 0 && !in_quote => {
                // Simple array comma (separates elements)
                if elem_start < i {
                    elements.push(&s[elem_start..i]);
                }
                i += 1;
                elem_start = i;
            }
            _ => {
                // Don't reset flag here - only reset at comma (new element) or brace (new subarray start)
                i += c.len_utf8();
            }
        }
    }

    // Handle remaining element after loop
    // Exclude trailing closing braces since they were consumed by the loop
    if elem_start < i {
        let end = if i > 0 && s.as_bytes()[i - 1] == b'}' {
            i - 1
        } else {
            i
        };
        if elem_start < end {
            elements.push(&s[elem_start..end]);
        }
    }

    // Handle final element (when loop ends without a trailing comma)
    // For nested arrays, we've already pushed all sub-arrays
    // For flat arrays, we need to push the last element
    if just_closed_subarray && elem_start < s.len() {
        // We've already closed a sub-array, but we might not have pushed the last one
        let remaining = s[elem_start..].trim();
        if !remaining.is_empty() {
            elements.push(remaining);
        }
    }

    Ok(elements)
}

/// Parses a single element from text, interpreting it as the given ColumnType.
fn parse_element_text(text: &str, elem_type: ColumnType) -> Result<crate::value::Value, DbError> {
    let text = text.trim();

    // NULL literal
    if text.eq_ignore_ascii_case("NULL") {
        return Ok(crate::value::Value::Null);
    }

    // Strip quotes if present
    let unquoted = if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        &text[1..text.len() - 1]
    } else {
        text
    };

    // Unescape backslash sequences
    let unescaped = unescape_string(unquoted);

    match elem_type {
        ColumnType::Bool => match unescaped.to_uppercase().as_str() {
            "TRUE" | "'t'" | "'1'" | "1" => Ok(crate::value::Value::Bool(true)),
            "FALSE" | "'f'" | "'0'" | "0" => Ok(crate::value::Value::Bool(false)),
            _ => Err(DbError::InvalidValue {
                reason: format!("invalid input syntax for type boolean: \"{}\"", text),
            }),
        },
        ColumnType::Int | ColumnType::Date => {
            // For Date, we store as i32 days. Parse as integer.
            let n: i64 = unescaped.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("invalid input syntax for type integer: \"{}\"", text),
            })?;
            let n = n.try_into().map_err(|_| DbError::InvalidValue {
                reason: format!("integer {} out of range for type", n),
            })?;
            Ok(crate::value::Value::Int(n))
        }
        ColumnType::BigInt => {
            let n: i64 = unescaped.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("invalid input syntax for type bigint: \"{}\"", text),
            })?;
            Ok(crate::value::Value::BigInt(n))
        }
        ColumnType::Float => {
            // Handle special values
            match unescaped.to_uppercase().as_str() {
                "INFINITY" | "INF" => Ok(crate::value::Value::Real(f64::INFINITY)),
                "-INFINITY" | "-INF" => Ok(crate::value::Value::Real(f64::NEG_INFINITY)),
                "NAN" => Err(DbError::InvalidValue {
                    reason: "NaN is not a valid SQL real value".to_string(),
                }),
                _ => {
                    let f: f64 = unescaped.parse().map_err(|_| DbError::InvalidValue {
                        reason: format!("invalid input syntax for type real: \"{}\"", text),
                    })?;
                    Ok(crate::value::Value::Real(f))
                }
            }
        }
        ColumnType::Decimal => {
            // PG decimal format: could be integer, or with decimal point, or scientific
            // For simplicity, parse as i128 mantissa with scale 0
            let m: i128 = unescaped.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("invalid input syntax for type decimal: \"{}\"", text),
            })?;
            Ok(crate::value::Value::Decimal(m, 0))
        }
        ColumnType::Text => Ok(crate::value::Value::Text(unescaped)),
        ColumnType::Json => {
            // Validate JSON
            serde_json::from_str::<serde_json::Value>(&unescaped).map_err(|e| {
                DbError::InvalidValue {
                    reason: format!("invalid input syntax for type json: \"{}\"", e),
                }
            })?;
            Ok(crate::value::Value::Json(unescaped))
        }
        ColumnType::Jsonb => {
            use crate::jsonb::JsonbEncoder;
            // Parse the unescaped string as a serde_json::Value first
            let json_value: serde_json::Value =
                serde_json::from_str(&unescaped).map_err(|e| DbError::InvalidValue {
                    reason: format!("invalid input syntax for type jsonb: \"{}\"", e),
                })?;
            let encoded = JsonbEncoder::encode(&json_value).map_err(|e| DbError::InvalidValue {
                reason: format!("invalid input syntax for type jsonb: \"{}\"", e),
            })?;
            Ok(crate::value::Value::Jsonb(std::sync::Arc::new(encoded)))
        }
        ColumnType::Bytes => {
            // PG bytea hex format: \\xHEX... or 0xHEX...
            let hex = if let Some(stripped) = unescaped.strip_prefix("\\x") {
                stripped
            } else if let Some(stripped) = unescaped.strip_prefix("0x") {
                stripped
            } else {
                return Err(DbError::InvalidValue {
                    reason: format!("invalid input syntax for type bytea: \"{}\"", text),
                });
            };
            let bytes = parse_hex_bytes(hex)?;
            Ok(crate::value::Value::Bytes(bytes))
        }
        ColumnType::Timestamp => {
            let t: i64 = unescaped.parse().map_err(|_| DbError::InvalidValue {
                reason: format!("invalid input syntax for type timestamp: \"{}\"", text),
            })?;
            Ok(crate::value::Value::Timestamp(t))
        }
        ColumnType::Uuid => {
            // Parse standard UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
            parse_uuid(&unescaped).map(crate::value::Value::Uuid)
        }
        ColumnType::Array => Err(DbError::InvalidValue {
            reason: "array element type cannot be array".to_string(),
        }),
    }
}

/// Unescapes backslash sequences in a quoted string.
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Parses a UUID string.
fn parse_uuid(s: &str) -> Result<[u8; 16], DbError> {
    let s = s.trim();
    // Simple UUID parsing: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return Err(DbError::InvalidValue {
            reason: format!("invalid UUID format: \"{}\"", s),
        });
    }

    let mut uuid = [0u8; 16];
    let sizes = [4, 2, 2, 2, 6];

    let mut offset = 0;
    for (i, part) in parts.iter().enumerate() {
        let bytes = hex::decode(*part).map_err(|_| DbError::InvalidValue {
            reason: format!("invalid UUID format: \"{}\" (bad hex at part {})", s, i),
        })?;
        if bytes.len() != sizes[i] {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "invalid UUID format: \"{}\" (part {} has {} bytes, expected {})",
                    s,
                    i,
                    bytes.len(),
                    sizes[i]
                ),
            });
        }
        uuid[offset..offset + bytes.len()].copy_from_slice(&bytes);
        offset += bytes.len();
    }

    Ok(uuid)
}

/// Parses a hex string into bytes.
fn parse_hex_bytes(hex: &str) -> Result<Vec<u8>, DbError> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err(DbError::InvalidValue {
            reason: format!("hex string must have even length, got: \"{}\"", hex),
        });
    }
    hex::decode(hex).map_err(|_| DbError::InvalidValue {
        reason: format!("invalid hex string: \"{}\"", hex),
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array_codec::encode_array;

    #[test]
    fn array_to_text_roundtrip_int() {
        let value = Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let elem_type = ColumnType::Int;
        let blob = encode_array(&value, elem_type).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{1,2,3}");
    }

    #[test]
    fn array_to_text_2d() {
        // 2x3 array: [[1,2,3],[4,5,6]]
        let value = Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
            Value::Int(6),
        ]);
        let blob = encode_array_nd(&value, ColumnType::Int, 2, &[2, 3]).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{{1,2,3},{4,5,6}}");
    }

    #[test]
    fn array_to_text_null_elements() {
        let value = Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]);
        let blob = encode_array(&value, ColumnType::Int).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{1,NULL,3}");
    }

    #[test]
    fn array_to_text_empty() {
        let value = Value::Array(vec![]);
        let blob = encode_array(&value, ColumnType::Int).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{}");
    }

    #[test]
    fn array_to_text_text_array() {
        let value = Value::Array(vec![
            Value::Text("hello".into()),
            Value::Text("world".into()),
        ]);
        let blob = encode_array(&value, ColumnType::Text).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{hello,world}");
    }

    #[test]
    fn array_to_text_quoted_element() {
        // Element with comma needs quoting
        let value = Value::Array(vec![Value::Text("hello, world".into())]);
        let blob = encode_array(&value, ColumnType::Text).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{\"hello, world\"}");
    }

    #[test]
    fn array_to_text_null_string() {
        // String "NULL" must be quoted
        let value = Value::Array(vec![Value::Text("NULL".into())]);
        let blob = encode_array(&value, ColumnType::Text).unwrap();
        let text = array_to_text(&blob).unwrap();
        assert_eq!(text, "{\"NULL\"}");
    }

    #[test]
    fn text_to_array_simple() {
        let blob = text_to_array("{1,2,3}", ColumnType::Int).unwrap();
        let (value, elem_type, ndim) = decode_array(&blob).unwrap();
        assert_eq!(ndim, 1);
        assert_eq!(elem_type, ColumnType::Int);
        assert_eq!(
            value,
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3),])
        );
    }

    #[test]
    fn text_to_array_empty() {
        let blob = text_to_array("{}", ColumnType::Int).unwrap();
        let (value, _, ndim) = decode_array(&blob).unwrap();
        assert_eq!(ndim, 0);
        assert_eq!(value, Value::Array(vec![]));
    }

    #[test]
    fn text_to_array_null_elements() {
        let blob = text_to_array("{1,NULL,3}", ColumnType::Int).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3),])
        );
    }

    #[test]
    fn text_to_array_text_with_null() {
        let blob = text_to_array("{NULL,\"NULL\",\"not null\"}", ColumnType::Text).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                Value::Null,
                Value::Text("NULL".into()),
                Value::Text("not null".into()),
            ])
        );
    }

    #[test]
    fn text_to_array_invalid_unclosed() {
        let result = text_to_array("{1,2", ColumnType::Int);
        assert!(result.is_err());
    }

    #[test]
    fn text_to_array_2d() {
        let blob = text_to_array("{{1,2,3},{4,5,6}}", ColumnType::Int).unwrap();
        let (value, _, ndim) = decode_array(&blob).unwrap();
        assert_eq!(ndim, 2);
        assert_eq!(
            value,
            Value::Array(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
                Value::Int(5),
                Value::Int(6),
            ])
        );
    }

    #[test]
    fn text_to_array_quoted() {
        let blob = text_to_array("{\"hello, world\"}", ColumnType::Text).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![Value::Text("hello, world".into())])
        );
    }

    #[test]
    fn text_to_array_bool() {
        let blob = text_to_array("{TRUE,FALSE}", ColumnType::Bool).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![Value::Bool(true), Value::Bool(false),])
        );
    }

    #[test]
    fn text_to_array_real() {
        let blob = text_to_array("{1.5,Infinity,-Infinity}", ColumnType::Float).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![
                Value::Real(1.5),
                Value::Real(f64::INFINITY),
                Value::Real(f64::NEG_INFINITY),
            ])
        );
    }

    #[test]
    fn text_to_array_uuid() {
        let blob =
            text_to_array("{12345678-9abc-def0-1234-56789abcdef0}", ColumnType::Uuid).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        let expected_uuid = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        assert_eq!(value, Value::Array(vec![Value::Uuid(expected_uuid)]));
    }

    #[test]
    fn text_to_array_bytea() {
        let blob = text_to_array(r"{0xDEADBEEF}", ColumnType::Bytes).unwrap();
        let (value, _, _) = decode_array(&blob).unwrap();
        assert_eq!(
            value,
            Value::Array(vec![Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])])
        );
    }

    #[test]
    fn roundtrip_mixed_types() {
        // Test roundtrip for various element types
        for (elem_type, values) in [
            (
                ColumnType::Bool,
                vec![Value::Bool(true), Value::Bool(false)],
            ),
            (
                ColumnType::Int,
                vec![Value::Int(-42), Value::Int(0), Value::Int(1000000)],
            ),
            (
                ColumnType::BigInt,
                vec![Value::BigInt(i64::MIN), Value::BigInt(i64::MAX)],
            ),
            (
                ColumnType::Float,
                vec![Value::Real(0.0), Value::Real(-3.14)],
            ),
            (
                ColumnType::Date,
                vec![Value::Date(-1), Value::Date(0), Value::Date(365)],
            ),
            (
                ColumnType::Timestamp,
                vec![
                    Value::Timestamp(-1_000_000),
                    Value::Timestamp(0),
                    Value::Timestamp(1_000_000),
                ],
            ),
        ] {
            let arr = Value::Array(values.clone());
            let blob = encode_array(&arr, elem_type).unwrap();
            let text = array_to_text(&blob).unwrap();
            let blob2 = text_to_array(&text, elem_type).unwrap();
            let (arr2, ct2, _) = decode_array(&blob2).unwrap();
            assert_eq!(ct2, elem_type);
            assert_eq!(arr2, arr, "roundtrip failed for {:?}", elem_type);
        }
    }
}
