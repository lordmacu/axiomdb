use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── String functions (4.19) ──────────────────────────────────────────
        "length" | "char_length" | "character_length" | "len" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.chars().count() as i32)),
                Value::Bytes(b) => Ok(Value::Int(b.len() as i32)),
                other => Err(DbError::TypeMismatch {
                    expected: "Text or Bytes".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        // OCTET_LENGTH / BYTE_LENGTH — returns byte count (not character count for TEXT).
        // Handles BLOB, TEXT, and UUID (always 16 bytes). Returns NULL for NULL input.
        // Extended in Phase 4.19b to cover UUID.
        "octet_length" | "byte_length" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.len() as i32)),
                Value::Bytes(b) => Ok(Value::Int(b.len() as i32)),
                Value::Uuid(_) => Ok(Value::Int(16)),
                other => Err(DbError::TypeMismatch {
                    expected: "Text or Bytes".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "upper" | "ucase" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_uppercase())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "lower" | "lcase" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_lowercase())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "trim" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "ltrim" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim_start().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "rtrim" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim_end().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "substr" | "substring" | "mid" => {
            // SUBSTR(str, start[, length]) — 1-based indexing (SQL standard)
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: "0".into(),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let start = if args.len() > 1 {
                match crate::eval::eval(&args[1], row)? {
                    Value::Int(n) => n as usize,
                    Value::BigInt(n) => n as usize,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "Int".into(),
                            got: other.variant_name().into(),
                        })
                    }
                }
            } else {
                1
            };
            let chars: Vec<char> = s.chars().collect();
            let start_idx = if start == 0 {
                0
            } else {
                (start - 1).min(chars.len())
            };
            let result = if args.len() > 2 {
                match crate::eval::eval(&args[2], row)? {
                    Value::Int(n) => chars[start_idx..]
                        .iter()
                        .take(n.max(0) as usize)
                        .collect::<String>(),
                    Value::BigInt(n) => chars[start_idx..]
                        .iter()
                        .take(n.max(0) as usize)
                        .collect::<String>(),
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "Int".into(),
                            got: other.variant_name().into(),
                        })
                    }
                }
            } else {
                chars[start_idx..].iter().collect::<String>()
            };
            Ok(Value::Text(result))
        }
        "concat" => {
            let mut result = String::new();
            for arg in args {
                match crate::eval::eval(arg, row)? {
                    Value::Null => {} // SQL CONCAT skips NULLs (MySQL behavior)
                    Value::Text(s) => result.push_str(&s),
                    Value::Int(n) => result.push_str(&n.to_string()),
                    Value::BigInt(n) => result.push_str(&n.to_string()),
                    Value::Real(f) => result.push_str(&f.to_string()),
                    other => result.push_str(&other.to_string()),
                }
            }
            Ok(Value::Text(result))
        }
        "concat_ws" => {
            // CONCAT_WS(separator, val1, val2, ...)
            if args.is_empty() {
                return Ok(Value::Text(String::new()));
            }
            let sep = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let mut parts: Vec<String> = Vec::new();
            for a in &args[1..] {
                match crate::eval::eval(a, row)? {
                    Value::Null => {} // skip NULLs
                    v => parts.push(v.to_string()),
                }
            }
            Ok(Value::Text(parts.join(&sep)))
        }
        "repeat" | "replicate" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let n = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            Ok(Value::Text(s.repeat(n)))
        }
        "replace" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let from = match crate::eval::eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let to = match crate::eval::eval(&args[2], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            Ok(Value::Text(s.replace(&from, &to)))
        }
        "reverse" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.chars().rev().collect())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "left" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let n = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            Ok(Value::Text(s.chars().take(n).collect()))
        }
        "right" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let n = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let chars: Vec<char> = s.chars().collect();
            let start = chars.len().saturating_sub(n);
            Ok(Value::Text(chars[start..].iter().collect()))
        }
        "lpad" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let len = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let pad = if args.len() > 2 {
                match crate::eval::eval(&args[2], row)? {
                    Value::Text(s) => s,
                    _ => " ".into(),
                }
            } else {
                " ".into()
            };
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= len {
                return Ok(Value::Text(chars[..len].iter().collect()));
            }
            let needed = len - chars.len();
            let pad_chars: Vec<char> = pad.chars().cycle().take(needed).collect();
            Ok(Value::Text(pad_chars.iter().chain(chars.iter()).collect()))
        }
        "rpad" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let len = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let pad = if args.len() > 2 {
                match crate::eval::eval(&args[2], row)? {
                    Value::Text(s) => s,
                    _ => " ".into(),
                }
            } else {
                " ".into()
            };
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= len {
                return Ok(Value::Text(chars[..len].iter().collect()));
            }
            let needed = len - chars.len();
            let pad_chars: Vec<char> = pad.chars().cycle().take(needed).collect();
            Ok(Value::Text(chars.iter().chain(pad_chars.iter()).collect()))
        }
        "locate" | "position" => {
            // LOCATE(needle, haystack) / POSITION(needle IN haystack) — same runtime form
            if args.len() < 2 {
                return Ok(Value::Int(0));
            }
            let needle = match crate::eval::eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            let haystack = match crate::eval::eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            Ok(match haystack.find(&needle[..]) {
                None => Value::Int(0),
                Some(byte_pos) => Value::Int(haystack[..byte_pos].chars().count() as i32 + 1),
            })
        }
        "instr" => {
            // INSTR(haystack, needle) — argument order reversed vs LOCATE
            if args.len() < 2 {
                return Ok(Value::Int(0));
            }
            let haystack = match crate::eval::eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            let needle = match crate::eval::eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            Ok(match haystack.find(&needle[..]) {
                None => Value::Int(0),
                Some(byte_pos) => Value::Int(haystack[..byte_pos].chars().count() as i32 + 1),
            })
        }
        "ascii" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.chars().next().map(|c| c as i32).unwrap_or(0))),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "char" | "chr" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Text(
                    char::from_u32(n as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                )),
                Value::BigInt(n) => Ok(Value::Text(
                    char::from_u32(n as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                )),
                other => Err(DbError::TypeMismatch {
                    expected: "Int".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "space" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Int(n) => Ok(Value::Text(" ".repeat(n.max(0) as usize))),
                Value::BigInt(n) => Ok(Value::Text(" ".repeat(n.max(0) as usize))),
                _ => Ok(Value::Text(String::new())),
            }
        }
        "strcmp" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = match crate::eval::eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let b = match crate::eval::eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }

        _ => unreachable!("dispatcher routed unsupported string function"),
    }
}
