//! Binary JSONB format — Phase 11.16.
//!
//! ## Layout (PostgreSQL-inspired)
//!
//! ```text
//! Container (object or array):
//!   [0..3]        u32 header: bit31=1→array, bits30..0=element count N
//!   [4..4+E*4-1]  JEntry array: E = 2*N (object) or N (array)
//!                   Object: key JEntries [0..N), value JEntries [N..2N)
//!   [4+E*4..]     Data section: key strings (sorted bytewise-length-first),
//!                   then value payloads
//!
//! Scalar wrapper (single scalar at the root):
//!   [0..3]        u32 header: JENTRY_FSCALAR | 1
//!   [4..7]        JEntry for the single element
//!   [8..]         payload bytes
//!
//! JEntry (u32 per element):
//!   bit31:        HAS_OFF — 0=length stored, 1=absolute offset from data start
//!   bits 30..28:  type field (0b000=string, 0b001=numeric, 0b010=false,
//!                              0b011=true, 0b100=null, 0b101=container)
//!   bits 27..0:   length OR absolute offset (max 256 MB)
//!
//! Stride: every JENTRY_STRIDE=32 entries use HAS_OFF to store an absolute offset.
//! This bounds element_offset(i) to O(STRIDE) = O(1).
//!
//! Key ordering: bytewise-length-first — compare length first, then bytes.
//! This allows binary search with a short-circuit comparator.
//! ```

use std::sync::Arc;

use axiomdb_core::error::DbError;

// ── Layout constants ──────────────────────────────────────────────────────────

pub const CONTAINER_IS_ARRAY: u32 = 0x8000_0000;
pub const CONTAINER_COUNT_MASK: u32 = 0x7FFF_FFFF;
/// Scalar wrapper flag (OR'd into count bits of header).
pub const JENTRY_FSCALAR: u32 = 0x0100_0000;

pub const JENTRY_HAS_OFF: u32 = 0x8000_0000;
pub const JENTRY_TYPE_MASK: u32 = 0x7000_0000;
pub const JENTRY_OFF_MASK: u32 = 0x0FFF_FFFF;

pub const JENTRY_ISSTRING: u32 = 0x0000_0000;
pub const JENTRY_ISNUMERIC: u32 = 0x1000_0000;
pub const JENTRY_ISFALSE: u32 = 0x2000_0000;
pub const JENTRY_ISTRUE: u32 = 0x3000_0000;
pub const JENTRY_ISNULL: u32 = 0x4000_0000;
pub const JENTRY_ISCONTAINER: u32 = 0x5000_0000;

pub const JENTRY_STRIDE: usize = 32;
const MAX_DEPTH: usize = 256;

// ── JsonbValue ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum JsonbValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Arc<str>),
    Container(Arc<Vec<u8>>),
}

impl JsonbValue {
    pub fn to_serde(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Int(n) => serde_json::json!(n),
            Self::Float(f) => serde_json::json!(f),
            Self::String(s) => serde_json::Value::String(s.as_ref().to_owned()),
            Self::Container(blob) => JsonbDecoder::decode(blob).unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Self::Container(_))
    }
}

// ── Encoder ───────────────────────────────────────────────────────────────────

pub struct JsonbEncoder;

impl JsonbEncoder {
    /// Encode a `serde_json::Value` into a binary JSONB blob (root entry point).
    /// Scalars are wrapped in a scalar container.
    pub fn encode(v: &serde_json::Value) -> Result<Vec<u8>, DbError> {
        let mut buf = Vec::new();
        match v {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                encode_container(v, &mut buf, 0)?;
            }
            _ => {
                // Scalar wrapper: header JENTRY_FSCALAR | 1, one JEntry, payload
                let header = JENTRY_FSCALAR | 1;
                buf.extend_from_slice(&header.to_le_bytes());
                let jentry_pos = buf.len();
                buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder
                let payload_start = buf.len();
                let jtype = write_scalar_payload(v, &mut buf)?;
                let payload_len = buf.len() - payload_start;
                let je = jtype | (payload_len as u32 & JENTRY_OFF_MASK);
                buf[jentry_pos..jentry_pos + 4].copy_from_slice(&je.to_le_bytes());
            }
        }
        Ok(buf)
    }
}

/// Encode any value inline (no scalar wrapper). Used for values inside containers.
/// Returns the JEntry type bits.
fn encode_inline(v: &serde_json::Value, buf: &mut Vec<u8>, depth: usize) -> Result<u32, DbError> {
    match v {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            encode_container(v, buf, depth + 1)?;
            Ok(JENTRY_ISCONTAINER)
        }
        _ => write_scalar_payload(v, buf),
    }
}

/// Encode an object or array container directly into `buf`.
fn encode_container(v: &serde_json::Value, buf: &mut Vec<u8>, depth: usize) -> Result<(), DbError> {
    if depth > MAX_DEPTH {
        return Err(DbError::InvalidValue {
            reason: format!("JSONB nesting depth exceeds maximum ({MAX_DEPTH})"),
        });
    }
    match v {
        serde_json::Value::Object(map) => encode_object(map, buf, depth),
        serde_json::Value::Array(arr) => encode_array(arr, buf, depth),
        _ => unreachable!("encode_container called with non-container"),
    }
}

fn encode_object(
    map: &serde_json::Map<String, serde_json::Value>,
    buf: &mut Vec<u8>,
    depth: usize,
) -> Result<(), DbError> {
    let n = map.len();

    // Sort keys: bytewise-length-first
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_by(|a, b| {
        a.len()
            .cmp(&b.len())
            .then_with(|| a.as_bytes().cmp(b.as_bytes()))
    });

    // Header: object (bit31=0), count=n
    let header: u32 = n as u32 & CONTAINER_COUNT_MASK;
    buf.extend_from_slice(&header.to_le_bytes());

    // Reserve 2*n JEntry slots (will be filled in later)
    let jentry_base = buf.len();
    buf.resize(buf.len() + n * 2 * 4, 0u8);

    // Write key strings to data section
    let data_start = buf.len();
    let mut key_lengths: Vec<usize> = Vec::with_capacity(n);
    for key in &keys {
        let bytes = key.as_bytes();
        key_lengths.push(bytes.len());
        buf.extend_from_slice(bytes);
    }

    // Write value payloads
    let mut value_lengths: Vec<usize> = Vec::with_capacity(n);
    let mut value_types: Vec<u32> = Vec::with_capacity(n);
    for key in &keys {
        let v = &map[*key];
        let start = buf.len();
        let jtype = encode_inline(v, buf, depth)?;
        value_lengths.push(buf.len() - start);
        value_types.push(jtype);
    }

    // Fill key JEntries (index_base=0)
    fill_jentries(
        buf,
        jentry_base,
        0,
        n,
        &vec![JENTRY_ISSTRING; n],
        &key_lengths,
    );

    // Fill value JEntries (index_base=n, so stride boundaries align correctly).
    // initial_running_offset = key_data_total so that stride entries in the
    // value section store globally-correct cumulative offsets.
    let key_data_total: usize = key_lengths.iter().sum();
    fill_jentries_with_offsets(
        buf,
        jentry_base + n * 4,
        data_start + key_data_total,
        key_data_total,
        n,
        n, // global index base for stride calculation
        &value_types,
        &value_lengths,
    );

    Ok(())
}

fn encode_array(arr: &[serde_json::Value], buf: &mut Vec<u8>, depth: usize) -> Result<(), DbError> {
    let n = arr.len();

    // Header: array (bit31=1), count=n
    let header: u32 = CONTAINER_IS_ARRAY | (n as u32 & CONTAINER_COUNT_MASK);
    buf.extend_from_slice(&header.to_le_bytes());

    // Reserve n JEntry slots
    let jentry_base = buf.len();
    buf.resize(buf.len() + n * 4, 0u8);

    // Write element payloads
    let mut elem_lengths: Vec<usize> = Vec::with_capacity(n);
    let mut elem_types: Vec<u32> = Vec::with_capacity(n);
    for v in arr {
        let start = buf.len();
        let jtype = encode_inline(v, buf, depth)?;
        elem_lengths.push(buf.len() - start);
        elem_types.push(jtype);
    }

    // Fill JEntries (index_base=0 for arrays)
    fill_jentries(buf, jentry_base, 0, n, &elem_types, &elem_lengths);

    Ok(())
}

/// Write scalar payload bytes to `buf`; return JEntry type bits.
fn write_scalar_payload(v: &serde_json::Value, buf: &mut Vec<u8>) -> Result<u32, DbError> {
    match v {
        serde_json::Value::Null => Ok(JENTRY_ISNULL),
        serde_json::Value::Bool(false) => Ok(JENTRY_ISFALSE),
        serde_json::Value::Bool(true) => Ok(JENTRY_ISTRUE),
        serde_json::Value::Number(n) => {
            buf.extend_from_slice(n.to_string().as_bytes());
            Ok(JENTRY_ISNUMERIC)
        }
        serde_json::Value::String(s) => {
            buf.extend_from_slice(s.as_bytes());
            Ok(JENTRY_ISSTRING)
        }
        _ => unreachable!("write_scalar_payload called with non-scalar"),
    }
}

/// Fill `n` JEntry slots starting at `buf[jentry_base]`, with index_base=0.
fn fill_jentries(
    buf: &mut [u8],
    jentry_base: usize,
    index_base: usize,
    n: usize,
    types: &[u32],
    lengths: &[usize],
) {
    fill_jentries_with_offsets(buf, jentry_base, 0, 0, n, index_base, types, lengths);
}

/// Fill `n` JEntry slots. Stride boundaries use the `global_index_base` for
/// determining when to switch to HAS_OFF.
///
/// `initial_running_offset`: cumulative byte offset from data section start
/// before this group of entries (e.g. `key_data_total` for value JEntries
/// in an object, so stride entries store globally-correct cumulative offsets).
fn fill_jentries_with_offsets(
    buf: &mut [u8],
    jentry_base: usize,
    _data_base: usize,
    initial_running_offset: usize,
    n: usize,
    global_index_base: usize,
    types: &[u32],
    lengths: &[usize],
) {
    let mut running_offset: usize = initial_running_offset;
    for i in 0..n {
        let global_idx = global_index_base + i;
        let len = lengths[i];
        let jtype = types[i];
        // Advance running_offset BEFORE the stride check so that stride
        // entries store the cumulative END offset (through this element).
        running_offset += len;
        let je = if global_idx > 0 && global_idx.is_multiple_of(JENTRY_STRIDE) {
            // Stride boundary: store cumulative end offset with HAS_OFF
            JENTRY_HAS_OFF | jtype | (running_offset as u32 & JENTRY_OFF_MASK)
        } else {
            // Normal: store length
            jtype | (len as u32 & JENTRY_OFF_MASK)
        };
        let slot = jentry_base + i * 4;
        buf[slot..slot + 4].copy_from_slice(&je.to_le_bytes());
    }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

pub struct JsonbDecoder;

impl JsonbDecoder {
    pub fn decode(data: &[u8]) -> Result<serde_json::Value, DbError> {
        if data.len() < 4 {
            return Err(DbError::ParseError {
                message: "JSONB blob too short (< 4 bytes)".into(),
                position: None,
            });
        }
        JsonbRef::new(data).to_serde_json()
    }

    pub fn to_string(data: &[u8]) -> Result<String, DbError> {
        Ok(Self::decode(data)?.to_string())
    }

    pub fn to_pretty_string(data: &[u8]) -> Result<String, DbError> {
        let v = Self::decode(data)?;
        serde_json::to_string_pretty(&v).map_err(|e| DbError::ParseError {
            message: format!("JSONB pretty-print failed: {e}"),
            position: None,
        })
    }
}

// ── JsonbRef ──────────────────────────────────────────────────────────────────

pub struct JsonbRef<'a> {
    data: &'a [u8],
}

impl<'a> JsonbRef<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    #[inline]
    pub fn header(&self) -> u32 {
        u32::from_le_bytes(self.data[0..4].try_into().unwrap())
    }

    #[inline]
    pub fn is_array(&self) -> bool {
        self.header() & CONTAINER_IS_ARRAY != 0
    }

    #[inline]
    pub fn is_scalar(&self) -> bool {
        self.header() & JENTRY_FSCALAR != 0
    }

    #[inline]
    pub fn element_count(&self) -> usize {
        (self.header() & CONTAINER_COUNT_MASK) as usize
    }

    #[inline]
    fn jentry_count(&self) -> usize {
        if self.is_scalar() {
            1
        } else if self.is_array() {
            self.element_count()
        } else {
            self.element_count() * 2
        }
    }

    #[inline]
    fn data_start(&self) -> usize {
        4 + self.jentry_count() * 4
    }

    #[inline]
    fn jentry_raw(&self, i: usize) -> u32 {
        let pos = 4 + i * 4;
        u32::from_le_bytes(self.data[pos..pos + 4].try_into().unwrap())
    }

    /// Compute byte offset (start) of element `i` from the data section start.
    /// Uses stride for O(STRIDE) = O(1) access.
    ///
    /// Stride entries store cumulative END offsets (through that element).
    /// To find start(i) we use the nearest stride AT OR BEFORE (i-1) as anchor,
    /// then sum forward.
    fn element_offset(&self, i: usize) -> usize {
        if i == 0 {
            return 0;
        }
        // Anchor: stride boundary at or before (i-1).
        let anchor = ((i - 1) / JENTRY_STRIDE) * JENTRY_STRIDE;
        let (base, start_j) = if anchor == 0 {
            // No stride entry before i — sum from element 0.
            (0usize, 0usize)
        } else {
            // anchor has HAS_OFF = cumulative end offset through anchor.
            let end_of_anchor = (self.jentry_raw(anchor) & JENTRY_OFF_MASK) as usize;
            (end_of_anchor, anchor + 1)
        };
        let mut offset = base;
        for j in start_j..i {
            let je = self.jentry_raw(j);
            // All entries in [start_j, i-1] are non-stride (no HAS_OFF).
            offset += (je & JENTRY_OFF_MASK) as usize;
        }
        offset
    }

    fn element_data(&self, i: usize) -> Result<&[u8], DbError> {
        let je = self.jentry_raw(i);
        let start_off = self.element_offset(i);
        let actual_len = if je & JENTRY_HAS_OFF != 0 {
            // Stride entry stores cumulative end offset; compute length
            // as start(i+1) - start(i).
            self.element_offset(i + 1) - start_off
        } else {
            (je & JENTRY_OFF_MASK) as usize
        };
        let start = self.data_start() + start_off;
        let end = start + actual_len;
        if end > self.data.len() {
            return Err(DbError::ParseError {
                message: format!(
                    "JSONB element {i} out of bounds: need {end}, have {}",
                    self.data.len()
                ),
                position: None,
            });
        }
        Ok(&self.data[start..end])
    }

    pub fn decode_element(&self, jentry_idx: usize) -> Result<JsonbValue, DbError> {
        let je = self.jentry_raw(jentry_idx);
        let jtype = je & JENTRY_TYPE_MASK;
        match jtype {
            JENTRY_ISNULL => Ok(JsonbValue::Null),
            JENTRY_ISFALSE => Ok(JsonbValue::Bool(false)),
            JENTRY_ISTRUE => Ok(JsonbValue::Bool(true)),
            JENTRY_ISSTRING => {
                let bytes = self.element_data(jentry_idx)?;
                let s = std::str::from_utf8(bytes).map_err(|_| DbError::ParseError {
                    message: format!("JSONB string at JEntry {jentry_idx} invalid UTF-8"),
                    position: None,
                })?;
                Ok(JsonbValue::String(Arc::from(s)))
            }
            JENTRY_ISNUMERIC => {
                let bytes = self.element_data(jentry_idx)?;
                let s = std::str::from_utf8(bytes).map_err(|_| DbError::ParseError {
                    message: format!("JSONB numeric at JEntry {jentry_idx} invalid UTF-8"),
                    position: None,
                })?;
                if let Ok(i) = s.parse::<i64>() {
                    Ok(JsonbValue::Int(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(JsonbValue::Float(f))
                } else {
                    Err(DbError::ParseError {
                        message: format!("JSONB numeric at JEntry {jentry_idx} invalid: {s}"),
                        position: None,
                    })
                }
            }
            JENTRY_ISCONTAINER => {
                let bytes = self.element_data(jentry_idx)?;
                Ok(JsonbValue::Container(Arc::new(bytes.to_vec())))
            }
            _ => Err(DbError::ParseError {
                message: format!("JSONB unknown type at JEntry {jentry_idx}: {:#010x}", je),
                position: None,
            }),
        }
    }

    /// Key lookup in an object using binary search on the sorted key section.
    /// Keys are sorted in bytewise-length-first order.
    /// Returns `None` if key not found; `Err` if the blob is malformed.
    pub fn get_key(&self, key: &str) -> Result<Option<JsonbValue>, DbError> {
        if self.is_array() || self.is_scalar() {
            return Ok(None);
        }
        let n = self.element_count();
        if n == 0 {
            return Ok(None);
        }
        let needle = key.as_bytes();

        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = self.element_data(mid)?;
            match Self::key_cmp(k, needle) {
                std::cmp::Ordering::Equal => {
                    return Ok(Some(self.decode_element(n + mid)?));
                }
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(None)
    }

    fn key_cmp(stored: &[u8], needle: &[u8]) -> std::cmp::Ordering {
        stored
            .len()
            .cmp(&needle.len())
            .then_with(|| stored.cmp(needle))
    }

    /// Array element access by index (negative = from end).
    pub fn get_index(&self, idx: i64) -> Result<Option<JsonbValue>, DbError> {
        if !self.is_array() {
            return Ok(None);
        }
        let n = self.element_count() as i64;
        let i = if idx < 0 { n + idx } else { idx };
        if i < 0 || i >= n {
            return Ok(None);
        }
        Ok(Some(self.decode_element(i as usize)?))
    }

    /// Return all keys in an object (in sorted order).
    pub fn object_keys(&self) -> Result<Vec<Arc<str>>, DbError> {
        if self.is_array() || self.is_scalar() {
            return Ok(vec![]);
        }
        let n = self.element_count();
        let mut keys = Vec::with_capacity(n);
        for i in 0..n {
            let bytes = self.element_data(i)?;
            let s = std::str::from_utf8(bytes).map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in JSONB key".into(),
                position: None,
            })?;
            keys.push(Arc::from(s));
        }
        Ok(keys)
    }

    /// Iterate key-value pairs in an object.
    pub fn object_iter(&self) -> Result<Vec<(Arc<str>, JsonbValue)>, DbError> {
        if self.is_array() || self.is_scalar() {
            return Ok(vec![]);
        }
        let n = self.element_count();
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            let key_bytes = self.element_data(i)?;
            let key = std::str::from_utf8(key_bytes).map_err(|_| DbError::ParseError {
                message: format!("invalid UTF-8 in JSONB key at index {i}"),
                position: None,
            })?;
            let value = self.decode_element(n + i)?;
            result.push((Arc::from(key), value));
        }
        Ok(result)
    }

    /// Iterate all elements in an array.
    pub fn array_iter(&self) -> Result<Vec<JsonbValue>, DbError> {
        if !self.is_array() {
            return Ok(vec![]);
        }
        let n = self.element_count();
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            result.push(self.decode_element(i)?);
        }
        Ok(result)
    }

    /// Convert to `serde_json::Value` (for mutation-path functions).
    pub fn to_serde_json(&self) -> Result<serde_json::Value, DbError> {
        let header = self.header();

        if header & JENTRY_FSCALAR != 0 {
            return self.decode_element(0).map(|v| v.to_serde());
        }

        if header & CONTAINER_IS_ARRAY != 0 {
            let n = self.element_count();
            let mut arr = Vec::with_capacity(n);
            for i in 0..n {
                arr.push(self.decode_element(i)?.to_serde());
            }
            return Ok(serde_json::Value::Array(arr));
        }

        // Object
        let n = self.element_count();
        let mut map = serde_json::Map::with_capacity(n);
        for i in 0..n {
            let key_bytes = self.element_data(i)?;
            let key = std::str::from_utf8(key_bytes)
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in JSONB key".into(),
                    position: None,
                })?
                .to_owned();
            let val = self.decode_element(n + i)?.to_serde();
            map.insert(key, val);
        }
        Ok(serde_json::Value::Object(map))
    }

    /// Compute maximum nesting depth (1 = scalar, 2 = flat obj/arr, ...).
    pub fn max_depth(&self) -> Result<usize, DbError> {
        self.max_depth_inner(1)
    }

    fn max_depth_inner(&self, current: usize) -> Result<usize, DbError> {
        if current > MAX_DEPTH {
            return Err(DbError::InvalidValue {
                reason: "JSONB depth exceeds maximum".into(),
            });
        }
        let header = self.header();
        if header & JENTRY_FSCALAR != 0 {
            return Ok(current);
        }

        let is_arr = header & CONTAINER_IS_ARRAY != 0;
        let n = self.element_count();
        if n == 0 {
            return Ok(current);
        }

        // For objects we check value JEntries (indices n..2n).
        // For arrays we check all JEntries (indices 0..n).
        let check_start = if is_arr { 0 } else { n };
        let check_end = if is_arr { n } else { n * 2 };

        let mut max_d = current;
        for i in check_start..check_end {
            let je = self.jentry_raw(i);
            if je & JENTRY_TYPE_MASK == JENTRY_ISCONTAINER {
                let child_bytes = self.element_data(i)?;
                let child = JsonbRef::new(child_bytes);
                let d = child.max_depth_inner(current + 1)?;
                if d > max_d {
                    max_d = d;
                }
            }
        }
        Ok(max_d)
    }
}

// ── Structural operations ─────────────────────────────────────────────────────

/// JSON_CONTAINS — structural subset check.
///
/// Rules:
/// - Objects: every key in `candidate` must exist in `doc` with a contained value.
/// - Arrays: every element in `candidate` must appear in `doc`.
/// - Scalars: equality.
pub fn jsonb_contains(doc: &[u8], candidate: &[u8]) -> Result<bool, DbError> {
    let doc_v = JsonbDecoder::decode(doc)?;
    let cand_v = JsonbDecoder::decode(candidate)?;
    Ok(serde_contains(&doc_v, &cand_v))
}

fn serde_contains(doc: &serde_json::Value, candidate: &serde_json::Value) -> bool {
    match (doc, candidate) {
        (serde_json::Value::Object(d), serde_json::Value::Object(c)) => c
            .iter()
            .all(|(k, cv)| d.get(k).is_some_and(|dv| serde_contains(dv, cv))),
        (serde_json::Value::Array(d), serde_json::Value::Array(c)) => {
            c.iter().all(|cv| d.iter().any(|dv| serde_contains(dv, cv)))
        }
        (d, c) => d == c,
    }
}

/// JSON_OVERLAPS — any shared element or key.
pub fn jsonb_overlaps(doc1: &[u8], doc2: &[u8]) -> Result<bool, DbError> {
    let v1 = JsonbDecoder::decode(doc1)?;
    let v2 = JsonbDecoder::decode(doc2)?;
    Ok(serde_overlaps(&v1, &v2))
}

fn serde_overlaps(v1: &serde_json::Value, v2: &serde_json::Value) -> bool {
    match (v1, v2) {
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            a.iter().any(|x| b.iter().any(|y| x == y))
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            a.keys().any(|k| b.contains_key(k))
        }
        (a, b) => a == b,
    }
}

/// JSON_MERGE_PATCH — RFC 7396 merge patch.
pub fn jsonb_merge_patch(doc: &serde_json::Value, patch: &serde_json::Value) -> serde_json::Value {
    match patch {
        serde_json::Value::Object(patch_map) => {
            let mut result = match doc {
                serde_json::Value::Object(m) => m.clone(),
                _ => serde_json::Map::new(),
            };
            for (k, pv) in patch_map {
                if pv.is_null() {
                    result.remove(k);
                } else {
                    let current = result.remove(k).unwrap_or(serde_json::Value::Null);
                    result.insert(k.clone(), jsonb_merge_patch(&current, pv));
                }
            }
            serde_json::Value::Object(result)
        }
        _ => patch.clone(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: serde_json::Value) -> Vec<u8> {
        JsonbEncoder::encode(&v).expect("encode failed")
    }

    fn dec(b: &[u8]) -> serde_json::Value {
        JsonbDecoder::decode(b).expect("decode failed")
    }

    fn rt(v: serde_json::Value) -> serde_json::Value {
        dec(&enc(v))
    }

    #[test]
    fn test_null() {
        assert_eq!(rt(serde_json::Value::Null), serde_json::Value::Null);
    }

    #[test]
    fn test_bool_true() {
        assert_eq!(rt(serde_json::json!(true)), serde_json::json!(true));
    }

    #[test]
    fn test_bool_false() {
        assert_eq!(rt(serde_json::json!(false)), serde_json::json!(false));
    }

    #[test]
    fn test_integer() {
        for v in [
            serde_json::json!(42),
            serde_json::json!(-100),
            serde_json::json!(i64::MAX),
        ] {
            assert_eq!(rt(v.clone()), v);
        }
    }

    #[test]
    fn test_float() {
        let v = serde_json::json!(3.14);
        assert_eq!(rt(v.clone()), v);
    }

    #[test]
    fn test_string_empty() {
        assert_eq!(rt(serde_json::json!("")), serde_json::json!(""));
    }

    #[test]
    fn test_string_utf8() {
        let v = serde_json::json!("café résumé 日本語");
        assert_eq!(rt(v.clone()), v);
    }

    #[test]
    fn test_empty_object() {
        assert_eq!(rt(serde_json::json!({})), serde_json::json!({}));
    }

    #[test]
    fn test_flat_object() {
        let v = serde_json::json!({"name": "Alice", "age": 30, "active": true});
        assert_eq!(rt(v.clone()), v);
    }

    #[test]
    fn test_nested_object() {
        let v = serde_json::json!({"user": {"id": 1, "profile": {"email": "alice@example.com"}}});
        assert_eq!(rt(v.clone()), v);
    }

    #[test]
    fn test_empty_array() {
        assert_eq!(rt(serde_json::json!([])), serde_json::json!([]));
    }

    #[test]
    fn test_array_mixed() {
        let v = serde_json::json!([null, true, false, 42, "hello", {"x": 1}]);
        assert_eq!(rt(v.clone()), v);
    }

    #[test]
    fn test_key_sort_order() {
        let v = serde_json::json!({"aa": 2, "a": 1, "b": 3, "ba": 5, "ab": 4});
        let blob = enc(v);
        let r = JsonbRef::new(&blob);
        let n = r.element_count();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..n {
            keys.push(r.element_data(i).unwrap().to_vec());
        }
        for w in keys.windows(2) {
            assert!(
                w[0].len() < w[1].len() || (w[0].len() == w[1].len() && w[0] <= w[1]),
                "keys not in bytewise-length-first order: {:?} vs {:?}",
                String::from_utf8_lossy(&w[0]),
                String::from_utf8_lossy(&w[1])
            );
        }
    }

    #[test]
    fn test_get_key_basic() {
        let v = serde_json::json!({"name": "Alice", "age": 30});
        let blob = enc(v);
        let r = JsonbRef::new(&blob);
        assert_eq!(
            r.get_key("name").unwrap().unwrap(),
            JsonbValue::String(Arc::from("Alice"))
        );
        assert_eq!(r.get_key("age").unwrap().unwrap(), JsonbValue::Int(30));
        assert!(r.get_key("missing").unwrap().is_none());
    }

    #[test]
    fn test_get_key_1000_keys() {
        let mut map = serde_json::Map::new();
        for i in 0..1000 {
            map.insert(format!("key{i:04}"), serde_json::json!(i));
        }
        let v = serde_json::Value::Object(map);
        let blob = enc(v);
        let r = JsonbRef::new(&blob);
        for i in 0..1000 {
            let key = format!("key{i:04}");
            let found = r.get_key(&key).unwrap();
            assert!(found.is_some(), "key {key} not found");
            assert_eq!(found.unwrap(), JsonbValue::Int(i as i64));
        }
        assert!(r.get_key("not_a_key").unwrap().is_none());
    }

    #[test]
    fn test_stride_boundary() {
        let mut map = serde_json::Map::new();
        for i in 0..33 {
            map.insert(format!("k{i:02}"), serde_json::json!(i));
        }
        let blob = enc(serde_json::Value::Object(map));
        let r = JsonbRef::new(&blob);
        // JEntry at index 32 (key[32]) should have HAS_OFF set
        let je32 = r.jentry_raw(32);
        assert_eq!(
            je32 & JENTRY_HAS_OFF,
            JENTRY_HAS_OFF,
            "JEntry[32] must have HAS_OFF set"
        );
        // The stored offset is the cumulative END offset through element 32,
        // i.e., sum of lengths 0..=32 (inclusive).
        let expected: usize = (0..32)
            .map(|i| (r.jentry_raw(i) & JENTRY_OFF_MASK) as usize)
            .sum::<usize>()
            + 3; // "k32" is 3 bytes — the length of the stride element itself
        assert_eq!((je32 & JENTRY_OFF_MASK) as usize, expected);
    }

    #[test]
    fn test_get_index() {
        let blob = enc(serde_json::json!(["a", "b", "c"]));
        let r = JsonbRef::new(&blob);
        assert_eq!(
            r.get_index(0).unwrap().unwrap(),
            JsonbValue::String(Arc::from("a"))
        );
        assert_eq!(
            r.get_index(2).unwrap().unwrap(),
            JsonbValue::String(Arc::from("c"))
        );
        assert_eq!(
            r.get_index(-1).unwrap().unwrap(),
            JsonbValue::String(Arc::from("c"))
        );
        assert!(r.get_index(10).unwrap().is_none());
    }

    #[test]
    fn test_merge_patch_add() {
        let doc = serde_json::json!({"a": 1});
        let patch = serde_json::json!({"b": 2});
        assert_eq!(
            jsonb_merge_patch(&doc, &patch),
            serde_json::json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn test_merge_patch_delete() {
        let doc = serde_json::json!({"a": 1, "b": 2});
        let patch = serde_json::json!({"b": null});
        assert_eq!(jsonb_merge_patch(&doc, &patch), serde_json::json!({"a": 1}));
    }

    #[test]
    fn test_merge_patch_replace_non_object() {
        let doc = serde_json::json!({"a": 1});
        let patch = serde_json::json!("replaced");
        assert_eq!(
            jsonb_merge_patch(&doc, &patch),
            serde_json::json!("replaced")
        );
    }

    #[test]
    fn test_contains() {
        let doc = enc(serde_json::json!({"a": 1, "b": {"c": 2}}));
        let yes = enc(serde_json::json!({"a": 1}));
        let no = enc(serde_json::json!({"a": 99}));
        assert!(jsonb_contains(&doc, &yes).unwrap());
        assert!(!jsonb_contains(&doc, &no).unwrap());
    }

    #[test]
    fn test_overlaps() {
        let a = enc(serde_json::json!(["x", "y"]));
        let b = enc(serde_json::json!(["y", "z"]));
        let c = enc(serde_json::json!(["w"]));
        assert!(jsonb_overlaps(&a, &b).unwrap());
        assert!(!jsonb_overlaps(&a, &c).unwrap());
    }

    #[test]
    fn test_max_depth() {
        let v = serde_json::json!({"a": {"b": {"c": 1}}});
        let blob = enc(v);
        let r = JsonbRef::new(&blob);
        assert_eq!(r.max_depth().unwrap(), 3);
    }
}

// ── GIN term extraction ───────────────────────────────────────────────────────

/// GIN term flags — compatible with PostgreSQL jsonb_ops (jsonb_gin.c JGINFLAG_*).
pub const GIN_FLAG_KEY: u8 = 0x01; // object key string, or string-typed array element
pub const GIN_FLAG_NULL: u8 = 0x02; // null value (no payload)
pub const GIN_FLAG_BOOL: u8 = 0x03; // bool (payload: 0=false, 1=true)
pub const GIN_FLAG_NUM: u8 = 0x04; // numeric (payload: canonical decimal string)
pub const GIN_FLAG_STR: u8 = 0x05; // string value that is NOT a key

/// Extract all GIN index terms from a JSONB document.
///
/// Each term is `[flag: u8][payload: bytes]`. The full set of terms from a
/// document is stored in the GIN B-Tree as separate entries. Two documents
/// satisfy `doc @> query` iff every term in `query`'s term set appears in
/// `doc`'s term set.
///
/// Extraction rules (mirrors `gin_extract_jsonb` in PostgreSQL `jsonb_gin.c`):
/// - Object key          → `[GIN_FLAG_KEY][key_utf8]`
/// - String array elem   → `[GIN_FLAG_KEY][elem_utf8]`  (PostgreSQL compat)
/// - Non-string array elem or object value → value term by type:
///   - null              → `[GIN_FLAG_NULL]`
///   - bool              → `[GIN_FLAG_BOOL][0|1]`
///   - int / float       → `[GIN_FLAG_NUM][decimal_string]`
///   - string            → `[GIN_FLAG_STR][utf8]`
///   - container         → recurse (no term for the container itself)
pub fn gin_extract_terms(data: &[u8]) -> Result<Vec<Vec<u8>>, DbError> {
    let root = JsonbDecoder::decode(data)?;
    let mut terms: Vec<Vec<u8>> = Vec::new();
    gin_collect(&root, &mut terms);
    // Deduplicate: the same key can appear in multiple sub-objects (e.g., "sku"
    // in two array items). Without dedup, inserting per-row GIN entries would
    // produce [term][0x00][pk_key] twice in the B-Tree → DuplicateKey.
    terms.sort_unstable();
    terms.dedup();
    Ok(terms)
}

/// Encode a single "key" term for the PG `?` / `?|` / `?&` operators.
///
/// The term layout is identical to the one used by `gin_collect` for object
/// keys and string array elements (`[GIN_FLAG_KEY][utf8]`), so `WHERE col ?
/// 'x'` can probe the existing GIN index created for `@>` without a separate
/// term kind. Mirrors PG `make_text_key(JGINFLAG_KEY, ...)` in
/// `jsonb_gin.c:874`.
pub fn gin_key_term(key: &str) -> Vec<u8> {
    let mut term = Vec::with_capacity(1 + key.len());
    term.push(GIN_FLAG_KEY);
    term.extend_from_slice(key.as_bytes());
    term
}

/// Extract GIN terms directly from a JSON text string (without binary encoding).
///
/// Convenience wrapper for planner-time extraction from JSON literal values.
pub fn gin_extract_terms_from_str(s: &str) -> Result<Vec<Vec<u8>>, DbError> {
    let root: serde_json::Value = serde_json::from_str(s).map_err(|e| DbError::InvalidValue {
        reason: format!("invalid JSON for GIN extraction: {e}"),
    })?;
    let mut terms: Vec<Vec<u8>> = Vec::new();
    gin_collect(&root, &mut terms);
    terms.sort_unstable();
    terms.dedup();
    Ok(terms)
}

fn gin_collect(val: &serde_json::Value, out: &mut Vec<Vec<u8>>) {
    match val {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                // key term
                let mut t = Vec::with_capacity(1 + k.len());
                t.push(GIN_FLAG_KEY);
                t.extend_from_slice(k.as_bytes());
                out.push(t);
                // value term (recurse for containers)
                gin_collect(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for elem in arr {
                match elem {
                    serde_json::Value::String(s) => {
                        // String array elements are treated as keys (PostgreSQL compat)
                        let mut t = Vec::with_capacity(1 + s.len());
                        t.push(GIN_FLAG_KEY);
                        t.extend_from_slice(s.as_bytes());
                        out.push(t);
                    }
                    other => gin_collect(other, out),
                }
            }
        }
        serde_json::Value::Null => out.push(vec![GIN_FLAG_NULL]),
        serde_json::Value::Bool(b) => out.push(vec![GIN_FLAG_BOOL, *b as u8]),
        serde_json::Value::Number(n) => {
            let s = n.to_string();
            let mut t = Vec::with_capacity(1 + s.len());
            t.push(GIN_FLAG_NUM);
            t.extend_from_slice(s.as_bytes());
            out.push(t);
        }
        serde_json::Value::String(s) => {
            let mut t = Vec::with_capacity(1 + s.len());
            t.push(GIN_FLAG_STR);
            t.extend_from_slice(s.as_bytes());
            out.push(t);
        }
    }
}

#[cfg(test)]
mod gin_tests {
    use super::*;

    fn enc_obj(j: serde_json::Value) -> Vec<u8> {
        JsonbEncoder::encode(&j).unwrap()
    }

    #[test]
    fn test_gin_extract_object_keys_and_values() {
        let blob = enc_obj(serde_json::json!({"name": "Alice", "age": 30}));
        let terms = gin_extract_terms(&blob).unwrap();
        // keys: "name", "age"
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"name" as &[u8]].concat()));
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"age" as &[u8]].concat()));
        // values: "Alice" (STR), 30 (NUM)
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_STR], b"Alice" as &[u8]].concat()));
        assert!(terms
            .iter()
            .any(|t| t[0] == GIN_FLAG_NUM && &t[1..] == b"30"));
    }

    #[test]
    fn test_gin_extract_string_array_elements_are_keys() {
        let blob = enc_obj(serde_json::json!(["a", "b", "c"]));
        let terms = gin_extract_terms(&blob).unwrap();
        // String array elements → KEY flag
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"a" as &[u8]].concat()));
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"b" as &[u8]].concat()));
    }

    #[test]
    fn test_gin_extract_bool_and_null() {
        let blob = enc_obj(serde_json::json!({"active": true, "deleted": null}));
        let terms = gin_extract_terms(&blob).unwrap();
        assert!(terms.iter().any(|t| t == &[GIN_FLAG_BOOL, 1]));
        assert!(terms.iter().any(|t| t == &[GIN_FLAG_NULL]));
    }

    #[test]
    fn test_gin_extract_nested_object() {
        let blob = enc_obj(serde_json::json!({"user": {"role": "admin"}}));
        let terms = gin_extract_terms(&blob).unwrap();
        // Both "user" key and inner "role" key + "admin" value
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"user" as &[u8]].concat()));
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_KEY], b"role" as &[u8]].concat()));
        assert!(terms
            .iter()
            .any(|t| t == &[&[GIN_FLAG_STR], b"admin" as &[u8]].concat()));
    }
}
