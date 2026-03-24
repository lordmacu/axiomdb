//! # axiomdb-types — Value, DataType, row codec, and type coercion
//!
//! - [`Value`] — typed SQL value in memory (executor representation)
//! - [`DataType`] — SQL column type descriptor (used by codec and executor)
//! - [`encode_row`] / [`decode_row`] / [`encoded_len`] — row binary codec
//! - [`coerce`] / [`coerce_for_op`] — type coercion matrix (Phase 4.18b)

pub mod codec;
pub mod coerce;
pub mod types;
pub mod value;

pub use codec::{decode_row, decode_row_masked, encode_row, encoded_len};
pub use coerce::{coerce, coerce_for_op, CoercionMode};
pub use types::DataType;
pub use value::Value;
