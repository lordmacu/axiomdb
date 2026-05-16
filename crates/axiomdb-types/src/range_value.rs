//! Range value type for SQL range types (Phase 20.13).
//!
//! Implements `int4range`, `int8range`, `numrange`, `daterange`, `tsrange`
//! following PostgreSQL semantics:
//!
//! - `[` = lower_inc (inclusive lower bound)
//! - `(` = exclusive lower bound
//! - `]` = upper_inc (inclusive upper bound)
//! - `)` = exclusive upper bound
//! - `None` bound = unbounded (−∞ for lower, +∞ for upper)
//! - `is_empty = true` = the canonical empty range (contains no points)
//!
//! ## Discrete canonicalization
//!
//! For discrete types (Int/BigInt/Date), inclusive upper bounds are
//! canonicalized to exclusive form on construction:
//! `[1,5]` → `[1,6)`, `[2024-01-01, 2024-01-05]` → `[2024-01-01, 2024-01-06)`.
//! This ensures `[1,5)` == `[1,4]` under `==`.

use axiomdb_core::error::DbError;

use crate::value::Value;

// ── Codec flags ───────────────────────────────────────────────────────────────

pub(crate) const RANGE_EMPTY: u8 = 0x01;
pub(crate) const RANGE_LOWER_BOUNDED: u8 = 0x02;
pub(crate) const RANGE_UPPER_BOUNDED: u8 = 0x04;
pub(crate) const RANGE_LOWER_INC: u8 = 0x08;
pub(crate) const RANGE_UPPER_INC: u8 = 0x10;

// ── RangeValue ────────────────────────────────────────────────────────────────

/// A SQL range value with optional lower/upper bounds.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeValue {
    /// Lower bound value; `None` = unbounded (−∞).
    pub lower: Option<Value>,
    /// Upper bound value; `None` = unbounded (+∞).
    pub upper: Option<Value>,
    /// `true` = inclusive lower (`[`), `false` = exclusive lower (`(`).
    pub lower_inc: bool,
    /// `true` = inclusive upper (`]`), `false` = exclusive upper (`)`).
    pub upper_inc: bool,
    /// `true` = the canonical empty range (contains no points).
    pub is_empty: bool,
}

impl RangeValue {
    /// The canonical empty range.
    pub fn empty() -> Self {
        Self {
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
            is_empty: true,
        }
    }

    /// Construct a range for continuous types (numrange, tsrange).
    /// No upper-bound canonicalization is applied.
    ///
    /// Returns `DbError::InvalidValue` if both bounds are present and lower > upper
    /// in a way that yields an empty range with incompatible inclusivity.
    pub fn new(
        lower: Option<Value>,
        upper: Option<Value>,
        lower_inc: bool,
        upper_inc: bool,
    ) -> Result<Self, DbError> {
        // Check for inverted ranges: lower > upper is always empty/invalid.
        if let (Some(ref lo), Some(ref hi)) = (&lower, &upper) {
            match compare_bounds(lo, hi) {
                Some(std::cmp::Ordering::Greater) => {
                    return Err(DbError::InvalidValue {
                        reason: "range lower bound must be less than or equal to upper bound"
                            .to_string(),
                    });
                }
                Some(std::cmp::Ordering::Equal) if !lower_inc || !upper_inc => {
                    // e.g. (1,1) or (1,1] or [1,1) are all empty for continuous types.
                    return Ok(Self::empty());
                }
                _ => {}
            }
        }
        Ok(Self {
            lower,
            upper,
            lower_inc,
            upper_inc,
            is_empty: false,
        })
    }

    /// Construct a range for discrete types (int4range, int8range, daterange).
    /// Canonicalizes inclusive upper bound to exclusive form.
    ///
    /// `increment_fn` advances the value by one discrete step.
    pub fn new_discrete(
        lower: Option<Value>,
        upper: Option<Value>,
        lower_inc: bool,
        upper_inc: bool,
        increment_fn: impl Fn(&Value) -> Result<Value, DbError>,
    ) -> Result<Self, DbError> {
        // Canonicalize exclusive lower → inclusive (PG: (1,5) → [2,5))
        let (canon_lower, canon_lower_inc) = match (lower, lower_inc) {
            (Some(ref lo), false) => (Some(increment_fn(lo)?), true),
            (l, inc) => (l, inc),
        };

        // Canonicalize inclusive upper → exclusive (PG: [1,5] → [1,6))
        let (canon_upper, canon_upper_inc) = match (upper, upper_inc) {
            (Some(ref hi), true) => (Some(increment_fn(hi)?), false),
            (u, inc) => (u, inc),
        };

        // After canonicalization: check for empty
        if let (Some(ref lo), Some(ref hi)) = (&canon_lower, &canon_upper) {
            match compare_bounds(lo, hi) {
                Some(std::cmp::Ordering::Greater) => {
                    return Ok(Self::empty());
                }
                Some(std::cmp::Ordering::Equal) if !canon_upper_inc => {
                    return Ok(Self::empty());
                }
                _ => {}
            }
        }

        Ok(Self {
            lower: canon_lower,
            upper: canon_upper,
            lower_inc: canon_lower_inc,
            upper_inc: canon_upper_inc,
            is_empty: false,
        })
    }

    /// Returns `true` if `point` lies within this range.
    pub fn contains_value(&self, point: &Value) -> bool {
        if self.is_empty {
            return false;
        }

        // Check lower bound
        if let Some(ref lo) = self.lower {
            match compare_bounds(lo, point) {
                Some(std::cmp::Ordering::Less) => {}
                Some(std::cmp::Ordering::Equal) => {
                    if !self.lower_inc {
                        return false;
                    }
                }
                Some(std::cmp::Ordering::Greater) => return false,
                None => return false,
            }
        }

        // Check upper bound
        if let Some(ref hi) = self.upper {
            match compare_bounds(point, hi) {
                Some(std::cmp::Ordering::Less) => {}
                Some(std::cmp::Ordering::Equal) => {
                    if !self.upper_inc {
                        return false;
                    }
                }
                Some(std::cmp::Ordering::Greater) => return false,
                None => return false,
            }
        }

        true
    }

    /// Returns `true` if `self` contains every point in `other`.
    pub fn contains_range(&self, other: &RangeValue) -> bool {
        if other.is_empty {
            return true; // empty is contained by everything
        }
        if self.is_empty {
            return false;
        }

        // Check lower: self.lower <= other.lower
        let lower_ok = match (&self.lower, &other.lower) {
            (None, _) => true,        // self is unbounded below
            (Some(_), None) => false, // other is unbounded but self isn't
            (Some(sl), Some(ol)) => match compare_bounds(sl, ol) {
                Some(std::cmp::Ordering::Less) => true,
                Some(std::cmp::Ordering::Equal) => self.lower_inc || !other.lower_inc,
                Some(std::cmp::Ordering::Greater) => false,
                None => false,
            },
        };
        if !lower_ok {
            return false;
        }

        // Check upper: self.upper >= other.upper
        match (&self.upper, &other.upper) {
            (None, _) => true, // self is unbounded above
            (Some(_), None) => false,
            (Some(su), Some(ou)) => match compare_bounds(su, ou) {
                Some(std::cmp::Ordering::Greater) => true,
                Some(std::cmp::Ordering::Equal) => self.upper_inc || !other.upper_inc,
                Some(std::cmp::Ordering::Less) | None => false,
            },
        }
    }

    /// Returns `true` if `self` and `other` share at least one point.
    pub fn overlaps(&self, other: &RangeValue) -> bool {
        if self.is_empty || other.is_empty {
            return false;
        }

        // self.lower < other.upper  (or equal with both inclusive)
        let self_starts_before_other_ends = match (&self.lower, &other.upper) {
            (None, _) | (_, None) => true,
            (Some(sl), Some(ou)) => match compare_bounds(sl, ou) {
                Some(std::cmp::Ordering::Less) => true,
                Some(std::cmp::Ordering::Equal) => self.lower_inc && other.upper_inc,
                Some(std::cmp::Ordering::Greater) => false,
                None => false,
            },
        };

        // other.lower < self.upper  (or equal with both inclusive)
        let other_starts_before_self_ends = match (&other.lower, &self.upper) {
            (None, _) | (_, None) => true,
            (Some(ol), Some(su)) => match compare_bounds(ol, su) {
                Some(std::cmp::Ordering::Less) => true,
                Some(std::cmp::Ordering::Equal) => other.lower_inc && self.upper_inc,
                Some(std::cmp::Ordering::Greater) => false,
                None => false,
            },
        };

        self_starts_before_other_ends && other_starts_before_self_ends
    }

    /// Returns the union of `self` and `other`, or `None` if the ranges are
    /// disjoint and non-adjacent (the result would be non-contiguous).
    pub fn union(&self, other: &RangeValue) -> Option<RangeValue> {
        if self.is_empty {
            return Some(other.clone());
        }
        if other.is_empty {
            return Some(self.clone());
        }
        if !self.overlaps(other) && !ranges_adjacent(self, other) {
            return None; // disjoint non-adjacent
        }

        let (new_lower, new_lower_inc) = pick_lower_bound(self, other);
        let (new_upper, new_upper_inc) = pick_upper_bound(self, other);

        Some(RangeValue {
            lower: new_lower,
            upper: new_upper,
            lower_inc: new_lower_inc,
            upper_inc: new_upper_inc,
            is_empty: false,
        })
    }

    /// Returns the intersection of `self` and `other`.
    /// Returns the empty range if the ranges are disjoint.
    pub fn intersection(&self, other: &RangeValue) -> RangeValue {
        if self.is_empty || other.is_empty || !self.overlaps(other) {
            return RangeValue::empty();
        }

        let (new_lower, new_lower_inc) = pick_intersection_lower(self, other);
        let (new_upper, new_upper_inc) = pick_intersection_upper(self, other);

        // Check if intersection is actually empty
        if let (Some(ref lo), Some(ref hi)) = (&new_lower, &new_upper) {
            match compare_bounds(lo, hi) {
                Some(std::cmp::Ordering::Greater) => return RangeValue::empty(),
                Some(std::cmp::Ordering::Equal) if !new_lower_inc || !new_upper_inc => {
                    return RangeValue::empty()
                }
                _ => {}
            }
        }

        RangeValue {
            lower: new_lower,
            upper: new_upper,
            lower_inc: new_lower_inc,
            upper_inc: new_upper_inc,
            is_empty: false,
        }
    }

    /// Returns `self - other`, or `None` if `other` is strictly interior to
    /// `self` (the result would be non-contiguous).
    pub fn difference(&self, other: &RangeValue) -> Option<RangeValue> {
        if self.is_empty {
            return Some(RangeValue::empty());
        }
        if other.is_empty || !self.overlaps(other) {
            return Some(self.clone());
        }

        // `other` fully swallows `self`
        if other.contains_range(self) {
            return Some(RangeValue::empty());
        }

        // Does `other` reach at or below `self`'s lower bound?
        // I.e., does self's lower point fall inside `other` (or other starts at -∞)?
        let other_eats_from_left = match (&other.lower, &self.lower) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(ol), Some(sl)) => match compare_bounds(ol, sl) {
                Some(std::cmp::Ordering::Less) => true,
                Some(std::cmp::Ordering::Equal) => other.lower_inc || !self.lower_inc,
                _ => false,
            },
        };

        // Does `other` reach at or above `self`'s upper bound?
        let other_eats_from_right = match (&other.upper, &self.upper) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(ou), Some(su)) => match compare_bounds(ou, su) {
                Some(std::cmp::Ordering::Greater) => true,
                Some(std::cmp::Ordering::Equal) => other.upper_inc || !self.upper_inc,
                _ => false,
            },
        };

        if other_eats_from_left {
            // other removes left part → result is right side: [other.upper, self.upper)
            return match &other.upper {
                None => Some(RangeValue::empty()),
                Some(hi) => Some(RangeValue {
                    lower: Some(hi.clone()),
                    upper: self.upper.clone(),
                    lower_inc: !other.upper_inc,
                    upper_inc: self.upper_inc,
                    is_empty: false,
                }),
            };
        }

        if other_eats_from_right {
            // other removes right part → result is left side: [self.lower, other.lower)
            return match &other.lower {
                None => Some(RangeValue::empty()),
                Some(lo) => Some(RangeValue {
                    lower: self.lower.clone(),
                    upper: Some(lo.clone()),
                    lower_inc: self.lower_inc,
                    upper_inc: !other.lower_inc,
                    is_empty: false,
                }),
            };
        }

        // `other` is strictly interior to `self` — result would be two disjoint pieces
        None
    }

    /// Canonical string representation (PostgreSQL format): `[lower,upper)`.
    pub fn to_display_string(&self) -> String {
        if self.is_empty {
            return "empty".to_string();
        }
        let lb = if self.lower_inc { '[' } else { '(' };
        let ub = if self.upper_inc { ']' } else { ')' };
        let lo = self
            .lower
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let hi = self
            .upper
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        format!("{lb}{lo},{hi}{ub}")
    }
}

// ── Bound comparison helper ───────────────────────────────────────────────────

/// Compare two bound values. Returns `None` if types are incomparable.
pub(crate) fn compare_bounds(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::BigInt(x), Value::BigInt(y)) => Some(x.cmp(y)),
        (Value::Decimal(mx, sx), Value::Decimal(my, sy)) => {
            // Compare as rational: mx * 10^(-sx) vs my * 10^(-sy)
            // Cross-multiply to avoid float: mx * 10^sy vs my * 10^sx
            let scale_diff = (*sx as i32) - (*sy as i32);
            let (lhs, rhs) = if scale_diff >= 0 {
                let factor = 10i128.pow(scale_diff as u32);
                (*my * factor, *mx)
            } else {
                let factor = 10i128.pow((-scale_diff) as u32);
                (*my, *mx * factor)
            };
            Some(rhs.cmp(&lhs))
        }
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::Timestamp(x), Value::Timestamp(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// ── Bound helpers ─────────────────────────────────────────────────────────────

fn ranges_adjacent(a: &RangeValue, b: &RangeValue) -> bool {
    // Two ranges are adjacent when one's upper touches the other's lower at the same
    // point and at least one side includes that point (no gap between them).
    // Example: [1,5) and [5,10) — upper=5 (excl) meets lower=5 (incl) → adjacent.
    // [1.0,5.0) and (5.0,10.0) — both exclude 5.0 → NOT adjacent (gap at 5.0).
    let touches =
        |upper: &Option<Value>, upper_inc: bool, lower: &Option<Value>, lower_inc: bool| {
            if let (Some(u), Some(l)) = (upper, lower) {
                if compare_bounds(u, l) == Some(std::cmp::Ordering::Equal) {
                    return upper_inc || lower_inc;
                }
            }
            false
        };
    touches(&a.upper, a.upper_inc, &b.lower, b.lower_inc)
        || touches(&b.upper, b.upper_inc, &a.lower, a.lower_inc)
}

fn pick_lower_bound(a: &RangeValue, b: &RangeValue) -> (Option<Value>, bool) {
    match (&a.lower, &b.lower) {
        (None, _) | (_, None) => (None, false),
        (Some(al), Some(bl)) => match compare_bounds(al, bl) {
            Some(std::cmp::Ordering::Less) => (a.lower.clone(), a.lower_inc),
            Some(std::cmp::Ordering::Greater) => (b.lower.clone(), b.lower_inc),
            Some(std::cmp::Ordering::Equal) => (a.lower.clone(), a.lower_inc || b.lower_inc),
            None => (a.lower.clone(), a.lower_inc),
        },
    }
}

fn pick_upper_bound(a: &RangeValue, b: &RangeValue) -> (Option<Value>, bool) {
    match (&a.upper, &b.upper) {
        (None, _) | (_, None) => (None, false),
        (Some(au), Some(bu)) => match compare_bounds(au, bu) {
            Some(std::cmp::Ordering::Greater) => (a.upper.clone(), a.upper_inc),
            Some(std::cmp::Ordering::Less) => (b.upper.clone(), b.upper_inc),
            Some(std::cmp::Ordering::Equal) => (a.upper.clone(), a.upper_inc || b.upper_inc),
            None => (a.upper.clone(), a.upper_inc),
        },
    }
}

fn pick_intersection_lower(a: &RangeValue, b: &RangeValue) -> (Option<Value>, bool) {
    match (&a.lower, &b.lower) {
        (None, other) => (other.clone(), b.lower_inc),
        (other, None) => (other.clone(), a.lower_inc),
        (Some(al), Some(bl)) => match compare_bounds(al, bl) {
            Some(std::cmp::Ordering::Greater) => (a.lower.clone(), a.lower_inc),
            Some(std::cmp::Ordering::Less) => (b.lower.clone(), b.lower_inc),
            Some(std::cmp::Ordering::Equal) => (a.lower.clone(), a.lower_inc && b.lower_inc),
            None => (a.lower.clone(), a.lower_inc),
        },
    }
}

fn pick_intersection_upper(a: &RangeValue, b: &RangeValue) -> (Option<Value>, bool) {
    match (&a.upper, &b.upper) {
        (None, other) => (other.clone(), b.upper_inc),
        (other, None) => (other.clone(), a.upper_inc),
        (Some(au), Some(bu)) => match compare_bounds(au, bu) {
            Some(std::cmp::Ordering::Less) => (a.upper.clone(), a.upper_inc),
            Some(std::cmp::Ordering::Greater) => (b.upper.clone(), b.upper_inc),
            Some(std::cmp::Ordering::Equal) => (a.upper.clone(), a.upper_inc && b.upper_inc),
            None => (a.upper.clone(), a.upper_inc),
        },
    }
}

// ── Range ordering ────────────────────────────────────────────────────────────

impl PartialOrd for RangeValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // empty < all non-empty
        match (self.is_empty, other.is_empty) {
            (true, true) => return Some(std::cmp::Ordering::Equal),
            (true, false) => return Some(std::cmp::Ordering::Less),
            (false, true) => return Some(std::cmp::Ordering::Greater),
            (false, false) => {}
        }

        // Compare lower bounds (unbounded = −∞ = smallest)
        let lo = compare_option_bounds(
            &self.lower,
            self.lower_inc,
            &other.lower,
            other.lower_inc,
            true,
        );
        if lo != std::cmp::Ordering::Equal {
            return Some(lo);
        }

        // Compare upper bounds (unbounded = +∞ = largest)
        Some(compare_option_bounds(
            &self.upper,
            self.upper_inc,
            &other.upper,
            other.upper_inc,
            false,
        ))
    }
}

fn compare_option_bounds(
    a: &Option<Value>,
    a_inc: bool,
    b: &Option<Value>,
    b_inc: bool,
    is_lower: bool,
) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => {
            if is_lower {
                std::cmp::Ordering::Less // −∞ < any finite
            } else {
                std::cmp::Ordering::Greater // +∞ > any finite
            }
        }
        (Some(_), None) => {
            if is_lower {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (Some(av), Some(bv)) => match compare_bounds(av, bv) {
            Some(std::cmp::Ordering::Equal) => {
                // inclusive > exclusive for lower; exclusive > inclusive for upper
                if is_lower {
                    b_inc.cmp(&a_inc) // [5,.. > (5,..  means lower_inc=true is "smaller" start
                } else {
                    a_inc.cmp(&b_inc) // ..,5] > ..,5)
                }
            }
            Some(ord) => ord,
            None => std::cmp::Ordering::Equal,
        },
    }
}

// ── Range literal parsing (for CAST / ::rangetype) ───────────────────────────

/// Parse a PostgreSQL range literal string into a `RangeValue`.
///
/// Accepts: `"empty"`, `"[lo,hi)"`, `"(lo,hi]"`, `"[,hi)"` (lower unbounded),
/// `"[lo,)"` (upper unbounded), `"(,)"` (all-inclusive unbounded).
///
/// `inner_type` is the element type (`DataType::Int`, `DataType::BigInt`, etc.).
/// `is_discrete` should be true for Int, BigInt, Date (canonical form required).
pub fn parse_range_literal_text(
    s: &str,
    inner_type: &crate::types::DataType,
) -> Result<RangeValue, DbError> {
    let is_discrete = matches!(
        inner_type,
        crate::types::DataType::Int | crate::types::DataType::BigInt | crate::types::DataType::Date
    );
    let s = s.trim();
    if s.eq_ignore_ascii_case("empty") {
        return Ok(RangeValue::empty());
    }
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return Err(DbError::ParseError {
            message: format!("invalid range literal '{s}'"),
            position: None,
        });
    }
    let lower_inc = match bytes[0] {
        b'[' => true,
        b'(' => false,
        _ => {
            return Err(DbError::ParseError {
                message: format!("range literal must start with '[' or '(': '{s}'"),
                position: None,
            })
        }
    };
    let upper_inc = match bytes[bytes.len() - 1] {
        b']' => true,
        b')' => false,
        _ => {
            return Err(DbError::ParseError {
                message: format!("range literal must end with ']' or ')': '{s}'"),
                position: None,
            })
        }
    };
    let inner = &s[1..s.len() - 1];
    let comma_pos = inner
        .as_bytes()
        .iter()
        .position(|&b| b == b',')
        .ok_or_else(|| DbError::ParseError {
            message: format!("range literal missing comma: '{s}'"),
            position: None,
        })?;
    let lo_str = inner[..comma_pos].trim();
    let hi_str = inner[comma_pos + 1..].trim();
    let lower = if lo_str.is_empty() {
        None
    } else {
        Some(parse_range_bound(lo_str, inner_type)?)
    };
    let upper = if hi_str.is_empty() {
        None
    } else {
        Some(parse_range_bound(hi_str, inner_type)?)
    };
    if is_discrete {
        RangeValue::new_discrete(lower, upper, lower_inc, upper_inc, |v| {
            increment_range_bound(v, inner_type)
        })
    } else {
        RangeValue::new(lower, upper, lower_inc, upper_inc)
    }
}

fn parse_range_bound(s: &str, inner_type: &crate::types::DataType) -> Result<Value, DbError> {
    use crate::coerce::{coerce, CoercionMode};
    coerce(
        Value::Text(s.to_string()),
        inner_type.clone(),
        CoercionMode::Strict,
    )
    .map_err(|_| DbError::ParseError {
        message: format!("invalid range bound '{s}' for type {}", inner_type.name()),
        position: None,
    })
}

fn increment_range_bound(v: &Value, inner_type: &crate::types::DataType) -> Result<Value, DbError> {
    match (v, inner_type) {
        (Value::Int(n), crate::types::DataType::Int) => n
            .checked_add(1)
            .map(Value::Int)
            .ok_or_else(|| DbError::InvalidValue {
                reason: "int4range upper bound overflow".to_string(),
            }),
        (Value::BigInt(n), crate::types::DataType::BigInt) => n
            .checked_add(1)
            .map(Value::BigInt)
            .ok_or_else(|| DbError::InvalidValue {
                reason: "int8range upper bound overflow".to_string(),
            }),
        (Value::Date(d), crate::types::DataType::Date) => d
            .checked_add(1)
            .map(Value::Date)
            .ok_or_else(|| DbError::InvalidValue {
                reason: "daterange upper bound overflow".to_string(),
            }),
        _ => Err(DbError::TypeMismatch {
            expected: "discrete range element (INT, BIGINT, DATE)".to_string(),
            got: v.variant_name().to_string(),
        }),
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_empty_construction() {
        let r = RangeValue::empty();
        assert!(r.is_empty);
        assert!(!r.lower_inc);
        assert!(!r.upper_inc);
        assert_eq!(r.lower, None);
        assert_eq!(r.upper, None);
    }

    #[test]
    fn range_int4_discrete_canonicalization() {
        let r =
            RangeValue::new_discrete(Some(Value::Int(1)), Some(Value::Int(5)), true, true, |v| {
                if let Value::Int(n) = v {
                    Ok(Value::Int(n + 1))
                } else {
                    unreachable!()
                }
            })
            .unwrap();
        assert_eq!(r.upper, Some(Value::Int(6)));
        assert!(!r.upper_inc);
        assert_eq!(r.lower, Some(Value::Int(1)));
        assert!(r.lower_inc);
    }

    #[test]
    fn range_exclusive_bounds_empty() {
        // (3,3) is empty even for continuous types
        let r = RangeValue::new(Some(Value::Int(3)), Some(Value::Int(3)), false, false).unwrap();
        assert!(r.is_empty);
    }

    #[test]
    fn range_contains_value_inclusive() {
        let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, false).unwrap();
        assert!(r.contains_value(&Value::Int(1)));
        assert!(r.contains_value(&Value::Int(5)));
        assert!(!r.contains_value(&Value::Int(10))); // exclusive upper
        assert!(!r.contains_value(&Value::Int(0)));
    }

    #[test]
    fn range_contains_value_fully_inclusive() {
        let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, true).unwrap();
        assert!(r.contains_value(&Value::Int(10)));
    }

    #[test]
    fn range_contains_value_unbounded_lower() {
        let r = RangeValue::new(None, Some(Value::Int(10)), false, false).unwrap();
        assert!(r.contains_value(&Value::Int(-1000)));
        assert!(r.contains_value(&Value::Int(9)));
        assert!(!r.contains_value(&Value::Int(10)));
    }

    #[test]
    fn range_contains_range() {
        let big = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(20)), true, false).unwrap();
        let small =
            RangeValue::new(Some(Value::Int(5)), Some(Value::Int(10)), true, false).unwrap();
        assert!(big.contains_range(&small));
        assert!(!small.contains_range(&big));
    }

    #[test]
    fn range_overlaps_true() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(3)), Some(Value::Int(8)), true, false).unwrap();
        assert!(a.overlaps(&b));
    }

    #[test]
    fn range_overlaps_false_disjoint() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        let c = RangeValue::new(Some(Value::Int(6)), Some(Value::Int(9)), true, false).unwrap();
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn range_overlaps_touching_exclusive() {
        // [1,5) and [5,10) don't overlap (5 is excluded from left range)
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(10)), true, false).unwrap();
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn range_union_overlapping() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(3)), Some(Value::Int(10)), true, false).unwrap();
        let u = a.union(&b).unwrap();
        assert_eq!(u.lower, Some(Value::Int(1)));
        assert_eq!(u.upper, Some(Value::Int(10)));
        assert!(!u.is_empty);
    }

    #[test]
    fn range_union_disjoint_returns_none() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(3)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(8)), true, false).unwrap();
        assert!(a.union(&b).is_none());
    }

    #[test]
    fn range_intersection_overlapping() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(15)), true, false).unwrap();
        let i = a.intersection(&b);
        assert_eq!(i.lower, Some(Value::Int(5)));
        assert_eq!(i.upper, Some(Value::Int(10)));
    }

    #[test]
    fn range_intersection_disjoint_empty() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(3)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(8)), true, false).unwrap();
        let i = a.intersection(&b);
        assert!(i.is_empty);
    }

    #[test]
    fn range_difference_non_overlapping() {
        let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        let b = RangeValue::new(Some(Value::Int(8)), Some(Value::Int(12)), true, false).unwrap();
        let d = a.difference(&b).unwrap();
        assert_eq!(d.lower, a.lower);
        assert_eq!(d.upper, a.upper);
    }

    #[test]
    fn range_ordering_empty_lt_nonempty() {
        let empty = RangeValue::empty();
        let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
        assert!(empty < r);
    }

    #[test]
    fn range_display_string() {
        let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, false).unwrap();
        assert_eq!(r.to_display_string(), "[1,10)");
        assert_eq!(RangeValue::empty().to_display_string(), "empty");
    }
}
