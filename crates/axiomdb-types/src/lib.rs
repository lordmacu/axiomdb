//! # axiomdb-types — Value, DataType, row codec, and type coercion
//!
//! - [`Value`] — typed SQL value in memory (executor representation)
//! - [`DataType`] — SQL column type descriptor (used by codec and executor)
//! - [`encode_row`] / [`decode_row`] / [`encoded_len`] — row binary codec
//! - [`coerce`] / [`coerce_for_op`] — type coercion matrix (Phase 4.18b)

pub mod array_codec;
pub mod array_io;
pub mod codec;
pub mod coerce;
pub mod field_patch;
pub mod jsonb;
pub mod range_value;
pub mod types;
pub mod value;

pub use array_codec::{decode_array, encode_array, encode_array_nd};
pub use array_io::{array_to_text, text_to_array};
pub use codec::{decode_row, decode_row_masked, encode_row, encoded_len};
pub use coerce::{coerce, coerce_for_op, CoercionMode};
pub use jsonb::{
    jsonb_contains, jsonb_merge_patch, jsonb_overlaps, JsonbDecoder, JsonbEncoder, JsonbRef,
    JsonbValue,
};
pub use range_value::RangeValue;
pub use types::DataType;
pub use value::Value;
