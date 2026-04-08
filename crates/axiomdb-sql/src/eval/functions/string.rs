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
            let start_raw: i64 = if args.len() > 1 {
                match crate::eval::eval(&args[1], row)? {
                    Value::Int(n) => n as i64,
                    Value::BigInt(n) => n,
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
            let start_idx = if start_raw < 0 {
                // MySQL: negative index counts from end; -1 = last char
                chars
                    .len()
                    .saturating_sub(start_raw.unsigned_abs() as usize)
            } else if start_raw == 0 {
                0
            } else {
                ((start_raw as usize) - 1).min(chars.len())
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
                    Value::Null => return Ok(Value::Null), // MySQL: any NULL arg → NULL result
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

        // ── Additional string functions (4.19j) ──────────────────────────────

        // NVL2(expr, val_if_not_null, val_if_null)
        "nvl2" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let e = crate::eval::eval(&args[0], row)?;
            if matches!(e, Value::Null) {
                crate::eval::eval(&args[2], row)
            } else {
                crate::eval::eval(&args[1], row)
            }
        }

        // INSERT(str, pos, len, newstr) — replace len chars starting at pos (1-indexed)
        "insert" => {
            if args.len() != 4 {
                return Err(DbError::TypeMismatch {
                    expected: "4 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match crate::eval::eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(t) => t,
                _ => return Ok(Value::Null),
            };
            let pos = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n,
                Value::BigInt(n) => n as i32,
                _ => return Ok(Value::Null),
            };
            let len = match crate::eval::eval(&args[2], row)? {
                Value::Int(n) => n,
                Value::BigInt(n) => n as i32,
                _ => return Ok(Value::Null),
            };
            let new = match crate::eval::eval(&args[3], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(t) => t,
                _ => return Ok(Value::Null),
            };
            if pos < 1 || pos as usize > s.len() {
                return Ok(Value::Text(s));
            }
            let char_pos = (pos - 1) as usize;
            let chars: Vec<char> = s.chars().collect();
            let end = (char_pos + len.max(0) as usize).min(chars.len());
            let mut result: String = chars[..char_pos].iter().collect();
            result.push_str(&new);
            result.extend(chars[end..].iter());
            Ok(Value::Text(result))
        }

        // ORD(str) → code point of first character
        "ord" => {
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
                _ => Ok(Value::Int(0)),
            }
        }

        // SOUNDEX(str) → phonetic key (Knuth algorithm)
        "soundex" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(soundex_encode(&s))),
                _ => Ok(Value::Null),
            }
        }

        // CHAR(n1, n2, ...) → string from code points (MySQL variadic form)
        "char" | "chr" => {
            if args.len() == 1 {
                let v = crate::eval::eval(&args[0], row)?;
                match v {
                    Value::Null => return Ok(Value::Null),
                    Value::Int(n) => {
                        return Ok(Value::Text(
                            char::from_u32(n as u32)
                                .map(|c| c.to_string())
                                .unwrap_or_default(),
                        ))
                    }
                    Value::BigInt(n) => {
                        return Ok(Value::Text(
                            char::from_u32(n as u32)
                                .map(|c| c.to_string())
                                .unwrap_or_default(),
                        ))
                    }
                    _ => return Ok(Value::Null),
                }
            }
            // Multi-arg form: concat all chars
            let mut out = String::new();
            for a in args {
                match crate::eval::eval(a, row)? {
                    Value::Null => {}
                    Value::Int(n) => {
                        if let Some(c) = char::from_u32(n as u32) {
                            out.push(c);
                        }
                    }
                    Value::BigInt(n) => {
                        if let Some(c) = char::from_u32(n as u32) {
                            out.push(c);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Value::Text(out))
        }

        // FORMAT(number, decimals) → formatted string with commas
        "format" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let v = crate::eval::eval(&args[0], row)?;
            let d = match crate::eval::eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let f = match v {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n as f64,
                Value::BigInt(n) => n as f64,
                Value::Real(f) => f,
                _ => return Ok(Value::Null),
            };
            let factor = 10f64.powi(d as i32);
            let rounded = (f * factor).round() / factor;
            let int_part = rounded.trunc().abs() as i64;
            let frac_part = (rounded.fract().abs() * factor).round() as u64;
            // Format integer part with commas
            let int_str = int_part.to_string();
            let mut with_commas = String::new();
            for (i, c) in int_str.chars().rev().enumerate() {
                if i > 0 && i % 3 == 0 {
                    with_commas.push(',');
                }
                with_commas.push(c);
            }
            let int_formatted: String = with_commas.chars().rev().collect();
            let sign = if f < 0.0 { "-" } else { "" };
            if d == 0 {
                Ok(Value::Text(format!("{}{}", sign, int_formatted)))
            } else {
                Ok(Value::Text(format!(
                    "{}{}.{:0>width$}",
                    sign,
                    int_formatted,
                    frac_part,
                    width = d
                )))
            }
        }

        // BIN(n) → binary representation
        "bin" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Text(format!("{:b}", n as u64))),
                Value::BigInt(n) => Ok(Value::Text(format!("{:b}", n as u64))),
                _ => Ok(Value::Null),
            }
        }

        // OCT(n) → octal representation
        "oct" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Text(format!("{:o}", n as u64))),
                Value::BigInt(n) => Ok(Value::Text(format!("{:o}", n as u64))),
                _ => Ok(Value::Null),
            }
        }

        // HEX(n or str) → hex string
        "hex" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Text(format!("{:X}", n as u64))),
                Value::BigInt(n) => Ok(Value::Text(format!("{:X}", n as u64))),
                Value::Text(s) => Ok(Value::Text(
                    s.bytes().map(|b| format!("{:02X}", b)).collect(),
                )),
                Value::Bytes(b) => Ok(Value::Text(
                    b.iter().map(|b| format!("{:02X}", b)).collect(),
                )),
                _ => Ok(Value::Null),
            }
        }

        // UNHEX(hex_str) → bytes
        "unhex" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let bytes: Option<Vec<u8>> = (0..s.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or(""), 16).ok())
                        .collect();
                    Ok(bytes.map(Value::Bytes).unwrap_or(Value::Null))
                }
                _ => Ok(Value::Null),
            }
        }

        // QUOTE(str) → SQL-quoted string with escaped single quotes
        "quote" => {
            let v = crate::eval::eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Text("NULL".into())),
                Value::Text(s) => Ok(Value::Text(format!(
                    "'{}'",
                    s.replace('\\', "\\\\").replace('\'', "\\'")
                ))),
                _ => Ok(Value::Null),
            }
        }

        // FIELD(val, v1, v2, ...) → 1-indexed position of val in list
        "field" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                });
            }
            let needle = crate::eval::eval(&args[0], row)?;
            for (i, a) in args[1..].iter().enumerate() {
                let v = crate::eval::eval(a, row)?;
                if crate::eval::ops::compare_values(&needle, &v)
                    .map(|o| o.is_eq())
                    .unwrap_or(false)
                {
                    return Ok(Value::Int((i + 1) as i32));
                }
            }
            Ok(Value::Int(0))
        }

        // ELT(n, s1, s2, ...) → nth string in list
        "elt" => {
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                });
            }
            let n = match crate::eval::eval(&args[0], row)? {
                Value::Int(n) => n,
                Value::BigInt(n) => n as i32,
                _ => return Ok(Value::Null),
            };
            if n < 1 || n as usize >= args.len() {
                return Ok(Value::Null);
            }
            crate::eval::eval(&args[n as usize], row)
        }

        _ => unreachable!("dispatcher routed unsupported string function"),
    }
}

/// Knuth Soundex algorithm (4-char code: letter + 3 digits).
fn soundex_encode(s: &str) -> String {
    let s = s.to_ascii_uppercase();
    let mut chars = s.chars().filter(|c| c.is_ascii_alphabetic());
    let first = match chars.next() {
        None => return "0000".to_string(),
        Some(c) => c,
    };
    let code_of = |c: char| match c {
        'B' | 'F' | 'P' | 'V' => '1',
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => '2',
        'D' | 'T' => '3',
        'L' => '4',
        'M' | 'N' => '5',
        'R' => '6',
        _ => '0',
    };
    let mut result = String::from(first);
    let mut prev = code_of(first);
    for c in chars {
        let code = code_of(c);
        if code != '0' && code != prev {
            result.push(code);
            if result.len() == 4 {
                break;
            }
        }
        prev = code;
    }
    while result.len() < 4 {
        result.push('0');
    }
    result
}
