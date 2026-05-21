//! Native N-API (napi-rs) binding for the AxiomDB embedded engine.
//!
//! Builds JS values (numbers, strings, buffers, arrays, objects) **directly in
//! Rust**, mirroring how `better-sqlite3` (a C++ addon) works — no per-cell FFI
//! marshalling like the koffi binding. Distributed as a single `.node` addon
//! (N-API is a stable ABI across Node versions).

#![deny(clippy::all)]

use napi::bindgen_prelude::*;
use napi::{Env, JsBigInt, JsBoolean, JsBuffer, JsNumber, JsString, JsUnknown, ValueType};
use napi_derive::napi;

use axiomdb_core::error::DbError;
use axiomdb_embedded::Db;
use axiomdb_types::Value;

fn to_napi_err(e: DbError) -> Error {
    Error::from_reason(e.to_string())
}

/// Converts a JS value into an AxiomDB [`Value`] for `?` parameter binding.
///
/// Mapping: null/undefined → Null, boolean → BigInt(0/1), integral number →
/// BigInt, fractional number → Real, bigint → BigInt, string → Text,
/// Buffer → Bytes.
fn js_to_value(v: &JsUnknown) -> Result<Value> {
    match v.get_type()? {
        ValueType::Null | ValueType::Undefined => Ok(Value::Null),
        ValueType::Boolean => {
            let b = unsafe { v.cast::<JsBoolean>() }.get_value()?;
            Ok(Value::BigInt(i64::from(b)))
        }
        ValueType::Number => {
            let f = unsafe { v.cast::<JsNumber>() }.get_double()?;
            // Integral, safe-range numbers bind as INT (matches the koffi path);
            // anything else is a REAL.
            if f.fract() == 0.0 && (-9_007_199_254_740_992.0..=9_007_199_254_740_992.0).contains(&f)
            {
                Ok(Value::BigInt(f as i64))
            } else {
                Ok(Value::Real(f))
            }
        }
        ValueType::BigInt => {
            let (val, _lossless) = unsafe { v.cast::<JsBigInt>() }.get_i64()?;
            Ok(Value::BigInt(val))
        }
        ValueType::String => {
            let s = unsafe { v.cast::<JsString>() }.into_utf8()?.into_owned()?;
            Ok(Value::Text(s))
        }
        ValueType::Object if v.is_buffer()? => {
            let buf = unsafe { v.cast::<JsBuffer>() }.into_value()?;
            Ok(Value::Bytes(buf.to_vec()))
        }
        other => Err(Error::from_reason(format!(
            "unsupported param type: {other:?}"
        ))),
    }
}

/// Converts an optional JS params array into engine [`Value`]s.
fn extract_params(params: &Option<Vec<JsUnknown>>) -> Result<Vec<Value>> {
    match params {
        None => Ok(Vec::new()),
        Some(p) => p.iter().map(js_to_value).collect(),
    }
}

fn format_uuid(u: &[u8; 16]) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
        u16::from_be_bytes([u[4], u[5]]),
        u16::from_be_bytes([u[6], u[7]]),
        u16::from_be_bytes([u[8], u[9]]),
        {
            let mut buf = [0u8; 8];
            buf[2..].copy_from_slice(&u[10..16]);
            u64::from_be_bytes(buf)
        }
    )
}

/// Converts an AxiomDB [`Value`] into a JS value. Mapping mirrors the other
/// bindings: int-likes → number, real/decimal → number, text/json/jsonb/uuid →
/// string, bytes → Buffer, null → null.
fn value_to_js(env: &Env, v: &Value) -> Result<JsUnknown> {
    Ok(match v {
        Value::Null => env.get_null()?.into_unknown(),
        Value::Bool(b) => env.get_boolean(*b)?.into_unknown(),
        Value::Int(i) => env.create_int32(*i)?.into_unknown(),
        Value::BigInt(i) => env.create_int64(*i)?.into_unknown(),
        Value::Real(f) => env.create_double(*f)?.into_unknown(),
        Value::Decimal(m, s) => env
            .create_double(*m as f64 / 10f64.powi(*s as i32))?
            .into_unknown(),
        Value::Date(d) => env.create_int64(*d as i64)?.into_unknown(),
        Value::Timestamp(t) | Value::TimestampTz(t) => env.create_int64(*t)?.into_unknown(),
        Value::Text(s) | Value::Json(s) => env.create_string(s)?.into_unknown(),
        Value::Bytes(b) => env.create_buffer_with_data(b.clone())?.into_unknown(),
        Value::Uuid(u) => env.create_string(&format_uuid(u))?.into_unknown(),
        Value::Jsonb(b) => env
            .create_string(
                &axiomdb_types::JsonbDecoder::to_string(b.as_ref())
                    .unwrap_or_else(|_| "null".to_string()),
            )?
            .into_unknown(),
        other => env.create_string(&other.to_string())?.into_unknown(),
    })
}

const PACKED_MAGIC: u32 = 0x4158_4d31; // "AXM1"

/// Serializes one value into `buf` using the packed cell encoding (matches the
/// C-FFI `axiomdb_query_packed` format so the JS parser is shared).
fn pack_value(buf: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => buf.push(0),
        Value::Bool(b) => {
            buf.push(1);
            buf.extend_from_slice(&(*b as i64).to_le_bytes());
        }
        Value::Int(i) => {
            buf.push(1);
            buf.extend_from_slice(&(*i as i64).to_le_bytes());
        }
        Value::BigInt(i) => {
            buf.push(1);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Date(d) => {
            buf.push(1);
            buf.extend_from_slice(&(*d as i64).to_le_bytes());
        }
        Value::Timestamp(t) | Value::TimestampTz(t) => {
            buf.push(1);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(2);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Decimal(m, s) => {
            buf.push(2);
            buf.extend_from_slice(&(*m as f64 / 10f64.powi(*s as i32)).to_le_bytes());
        }
        // Tag 5 (ASCII) lets JS decode with latin1 (no UTF-8 validation, ~20%
        // faster string creation); tag 3 stays UTF-8 for everything else.
        Value::Text(s) | Value::Json(s) => {
            if s.is_ascii() {
                pack_ascii(buf, s.as_bytes());
            } else {
                pack_text(buf, s.as_bytes());
            }
        }
        Value::Bytes(b) => {
            buf.push(4);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Uuid(u) => pack_text(buf, format_uuid(u).as_bytes()),
        Value::Jsonb(b) => pack_text(
            buf,
            axiomdb_types::JsonbDecoder::to_string(b.as_ref())
                .unwrap_or_else(|_| "null".to_string())
                .as_bytes(),
        ),
        other => pack_text(buf, other.to_string().as_bytes()),
    }
}

fn pack_text(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.push(3);
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// ASCII text — tag 5; the JS side decodes with latin1.
fn pack_ascii(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.push(5);
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn pack_result(cols: &[String], rows: &[Vec<Value>]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + cols.len() * 16 + rows.len() * cols.len() * 12);
    buf.extend_from_slice(&PACKED_MAGIC.to_le_bytes());
    buf.extend_from_slice(&(cols.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for c in cols {
        buf.extend_from_slice(&(c.len() as u32).to_le_bytes());
        buf.extend_from_slice(c.as_bytes());
    }
    for row in rows {
        for v in row {
            pack_value(&mut buf, v);
        }
    }
    buf
}

/// An in-process AxiomDB connection.
#[napi]
pub struct Connection {
    db: Option<Db>,
}

#[napi]
impl Connection {
    /// Opens or creates a database at `path` (`":memory:"` for ephemeral).
    #[napi(constructor)]
    pub fn new(path: String) -> Result<Self> {
        let db = Db::open(&path).map_err(to_napi_err)?;
        Ok(Self { db: Some(db) })
    }

    fn db_mut(&mut self) -> Result<&mut Db> {
        self.db
            .as_mut()
            .ok_or_else(|| Error::from_reason("connection is closed"))
    }

    /// Executes a DDL/DML statement. Returns rows affected.
    ///
    /// Pass `params` (an array) to bind `?` placeholders with real
    /// prepared-statement binding — no string interpolation, no SQL injection.
    #[napi]
    pub fn execute(&mut self, sql: String, params: Option<Vec<JsUnknown>>) -> Result<i64> {
        if params.is_none() {
            return Ok(self.db_mut()?.execute(&sql).map_err(to_napi_err)? as i64);
        }
        let values = extract_params(&params)?;
        Ok(self
            .db_mut()?
            .execute_params(&sql, &values)
            .map_err(to_napi_err)? as i64)
    }

    /// Executes a SELECT and returns rows as arrays (fastest; matches
    /// better-sqlite3's `.raw().all()` shape). Pass `params` to bind `?`.
    #[napi]
    pub fn query_tuples(
        &mut self,
        env: Env,
        sql: String,
        params: Option<Vec<JsUnknown>>,
    ) -> Result<Array> {
        let rows = if params.is_none() {
            self.db_mut()?.query(&sql).map_err(to_napi_err)?
        } else {
            let values = extract_params(&params)?;
            self.db_mut()?
                .query_params(&sql, &values)
                .map_err(to_napi_err)?
                .1
        };
        let mut out = env.create_array(rows.len() as u32)?;
        for (i, row) in rows.iter().enumerate() {
            let mut r = env.create_array(row.len() as u32)?;
            for (j, v) in row.iter().enumerate() {
                r.set(j as u32, value_to_js(&env, v)?)?;
            }
            out.set(i as u32, r)?;
        }
        Ok(out)
    }

    /// Executes a SELECT and returns rows as objects (column name → value).
    /// Pass `params` to bind `?` placeholders safely.
    #[napi]
    pub fn query(&mut self, env: Env, sql: String, params: Option<Vec<JsUnknown>>) -> Result<Array> {
        let (cols, rows) = if params.is_none() {
            self.db_mut()?
                .query_with_columns(&sql)
                .map_err(to_napi_err)?
        } else {
            let values = extract_params(&params)?;
            self.db_mut()?
                .query_params(&sql, &values)
                .map_err(to_napi_err)?
        };
        let mut out = env.create_array(rows.len() as u32)?;
        for (i, row) in rows.iter().enumerate() {
            let mut obj = env.create_object()?;
            for (j, v) in row.iter().enumerate() {
                obj.set_named_property(&cols[j], value_to_js(&env, v)?)?;
            }
            out.set(i as u32, obj)?;
        }
        Ok(out)
    }

    /// Executes a SELECT and returns the whole result as one packed `Buffer`
    /// (the JS side parses it). Avoids the per-cell N-API object construction of
    /// `queryTuples`/`query`, which dominates for large results. Pass `params`
    /// to bind `?` placeholders safely.
    #[napi]
    pub fn query_packed(&mut self, sql: String, params: Option<Vec<JsUnknown>>) -> Result<Buffer> {
        let (cols, rows) = if params.is_none() {
            self.db_mut()?
                .query_with_columns(&sql)
                .map_err(to_napi_err)?
        } else {
            let values = extract_params(&params)?;
            self.db_mut()?
                .query_params(&sql, &values)
                .map_err(to_napi_err)?
        };
        Ok(pack_result(&cols, &rows).into())
    }

    /// Begins an explicit transaction.
    #[napi]
    pub fn begin(&mut self) -> Result<()> {
        self.db_mut()?.begin().map_err(to_napi_err)
    }

    /// Commits the active explicit transaction.
    #[napi]
    pub fn commit(&mut self) -> Result<()> {
        self.db_mut()?.commit().map_err(to_napi_err)
    }

    /// Rolls back the active explicit transaction.
    #[napi]
    pub fn rollback(&mut self) -> Result<()> {
        self.db_mut()?.rollback().map_err(to_napi_err)
    }

    /// Closes the connection. Safe to call multiple times.
    #[napi]
    pub fn close(&mut self) {
        self.db = None;
    }
}
