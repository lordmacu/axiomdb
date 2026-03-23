//! # nexusdb-types — Value, DataType, and row codec
//!
//! - [`Value`] — typed SQL value in memory (executor representation)
//! - [`DataType`] — SQL column type descriptor (used by codec and executor)
//! - [`encode_row`] / [`decode_row`] / [`encoded_len`] — row binary codec

pub mod codec;
pub mod types;
pub mod value;

pub use codec::{decode_row, encode_row, encoded_len};
pub use types::DataType;
pub use value::Value;
