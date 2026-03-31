//! Literal-normalized COM_QUERY plan cache (Phase 27.8b).
//!
//! For repeated ad-hoc queries like `SELECT * FROM users WHERE id = 42`,
//! normalizes the SQL by replacing literal values with `?` placeholders,
//! then caches the parsed+analyzed AST. Subsequent queries with the same
//! structure but different literals (e.g., `id = 43`) skip parse+analyze
//! and reuse the cached plan.
//!
//! ## Design reference
//!
//! Inspired by PostgreSQL's plan cache: PG caches per-prepared-statement
//! and uses cost-based generic vs. custom plan decision. AxiomDB's approach
//! is simpler: normalize literals at the SQL string level, hash the result,
//! and cache the analyzed Stmt per-connection.
//!
//! ## Normalization rules
//!
//! - Integer literals → `?`  (extract as Value::Int / Value::BigInt)
//! - Float literals → `?`    (extract as Value::Real)
//! - String literals → `?`   (extract as Value::Text)
//! - Other tokens → unchanged
//! - Identifiers, keywords, operators → preserved exactly

use std::collections::HashMap;

use axiomdb_core::error::DbError;
use axiomdb_sql::ast::Stmt;
use axiomdb_types::Value;

/// Per-connection plan cache for normalized COM_QUERY statements.
pub struct PlanCache {
    entries: HashMap<u64, CachedPlan>,
    schema_version: u64,
    max_entries: usize,
}

struct CachedPlan {
    stmt: Stmt,
    param_count: usize,
}

impl PlanCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            schema_version: 0,
            max_entries,
        }
    }

    /// Normalizes the SQL, checks the cache, returns (cached_stmt, params) on hit.
    /// Returns None on cache miss.
    pub fn lookup(&self, sql: &str, current_schema_version: u64) -> Option<(Stmt, Vec<Value>)> {
        // Schema version changed → entire cache is stale.
        if self.schema_version != current_schema_version {
            return None;
        }

        let (normalized, params) = normalize_sql(sql);
        let key = fnv1a_hash(normalized.as_bytes());

        self.entries.get(&key).and_then(|cached| {
            if cached.param_count == params.len() {
                let stmt = cached.stmt.clone();
                match substitute_params(stmt, &params) {
                    Ok(substituted) => Some((substituted, params)),
                    Err(_) => None,
                }
            } else {
                None // Different param count → structure mismatch
            }
        })
    }

    /// Stores a parsed+analyzed Stmt in the cache.
    pub fn store(&mut self, sql: &str, stmt: &Stmt, current_schema_version: u64) {
        // Invalidate on DDL.
        if self.schema_version != current_schema_version {
            self.entries.clear();
            self.schema_version = current_schema_version;
        }

        let (normalized, params) = normalize_sql(sql);
        let key = fnv1a_hash(normalized.as_bytes());

        // Evict oldest if at capacity (simple: just clear half).
        if self.entries.len() >= self.max_entries {
            self.entries.clear();
        }

        self.entries.insert(
            key,
            CachedPlan {
                stmt: stmt.clone(),
                param_count: params.len(),
            },
        );
    }
}

// ── SQL normalization ────────────────────────────────────────────────────────

/// Replaces literal values in SQL with `?` placeholders, extracting the values.
///
/// Handles:
/// - Integer literals: `42` → `?` + Value::Int(42)
/// - Negative integers: only inside WHERE/SET context (heuristic)
/// - Float literals: `3.14` → `?` + Value::Real(3.14)
/// - String literals: `'hello'` → `?` + Value::Text("hello")
///
/// Does NOT normalize:
/// - Identifiers, keywords, operators
/// - Numbers that are part of identifiers (e.g., table1)
/// - Negative signs that are unary operators
pub fn normalize_sql(sql: &str) -> (String, Vec<Value>) {
    let mut result = String::with_capacity(sql.len());
    let mut params = Vec::new();
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let b = bytes[i];

        // Skip whitespace.
        if b.is_ascii_whitespace() {
            result.push(b as char);
            i += 1;
            continue;
        }

        // String literal: 'content'
        if b == b'\'' {
            i += 1;
            let mut s = String::new();
            while i < len {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        s.push('\''); // escaped quote
                        i += 2;
                    } else {
                        i += 1; // closing quote
                        break;
                    }
                } else {
                    s.push(bytes[i] as char);
                    i += 1;
                }
            }
            result.push('?');
            params.push(Value::Text(s));
            continue;
        }

        // Number literal: starts with digit or '.' followed by digit.
        if b.is_ascii_digit() || (b == b'.' && i + 1 < len && bytes[i + 1].is_ascii_digit()) {
            let num_start = i;
            let mut is_float = b == b'.';
            i += 1;
            while i < len
                && (bytes[i].is_ascii_digit()
                    || bytes[i] == b'.'
                    || bytes[i] == b'e'
                    || bytes[i] == b'E')
            {
                if bytes[i] == b'.' || bytes[i] == b'e' || bytes[i] == b'E' {
                    is_float = true;
                }
                i += 1;
            }

            // Check if this number is part of an identifier (e.g., "table1").
            // If preceded by a letter or underscore, it's part of an identifier.
            if num_start > 0
                && (bytes[num_start - 1].is_ascii_alphanumeric() || bytes[num_start - 1] == b'_')
            {
                // Part of identifier — keep as-is.
                result.push_str(&sql[num_start..i]);
                continue;
            }

            let num_str = &sql[num_start..i];
            if is_float {
                if let Ok(f) = num_str.parse::<f64>() {
                    result.push('?');
                    params.push(Value::Real(f));
                } else {
                    result.push_str(num_str);
                }
            } else if let Ok(n) = num_str.parse::<i64>() {
                result.push('?');
                if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                    params.push(Value::Int(n as i32));
                } else {
                    params.push(Value::BigInt(n));
                }
            } else {
                result.push_str(num_str);
            }
            continue;
        }

        // Everything else: copy as-is.
        result.push(b as char);
        i += 1;
    }

    (result, params)
}

/// Substitutes `Expr::Param { idx }` nodes in the AST with literal values.
/// Uses the existing prepared statement substitution from `prepared.rs`.
fn substitute_params(stmt: Stmt, params: &[Value]) -> Result<Stmt, DbError> {
    super::prepared::substitute_params_in_ast(stmt, params)
}

/// FNV-1a hash for cache keys.
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_integer() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE id = 42");
        assert_eq!(norm, "SELECT * FROM t WHERE id = ?");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], Value::Int(42));
    }

    #[test]
    fn test_normalize_string() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE name = 'alice'");
        assert_eq!(norm, "SELECT * FROM t WHERE name = ?");
        assert_eq!(params, vec![Value::Text("alice".into())]);
    }

    #[test]
    fn test_normalize_multiple_literals() {
        let (norm, params) = normalize_sql("INSERT INTO t VALUES (1, 'hello', 3.14)");
        assert_eq!(norm, "INSERT INTO t VALUES (?, ?, ?)");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], Value::Int(1));
        assert_eq!(params[1], Value::Text("hello".into()));
        assert!(matches!(params[2], Value::Real(f) if (f - 3.14).abs() < 0.001));
    }

    #[test]
    fn test_normalize_preserves_identifiers() {
        let (norm, _) = normalize_sql("SELECT col1 FROM table2 WHERE id = 5");
        // "table2" should NOT have its "2" replaced.
        assert!(norm.contains("table2"));
        assert!(norm.contains("col1"));
    }

    #[test]
    fn test_normalize_escaped_string() {
        let (norm, params) = normalize_sql("SELECT * FROM t WHERE s = 'it''s'");
        assert_eq!(norm, "SELECT * FROM t WHERE s = ?");
        assert_eq!(params, vec![Value::Text("it's".into())]);
    }

    #[test]
    fn test_cache_hit() {
        let mut cache = PlanCache::new(100);
        // Simulate: first query parsed, stored in cache.
        let sql1 = "SELECT * FROM t WHERE id = 42";
        let stmt = axiomdb_sql::parser::parse(sql1, None).unwrap();
        cache.store(sql1, &stmt, 1);

        // Second query with different literal: should hit.
        let sql2 = "SELECT * FROM t WHERE id = 99";
        let result = cache.lookup(sql2, 1);
        assert!(result.is_some());
    }

    #[test]
    fn test_cache_miss_different_structure() {
        let mut cache = PlanCache::new(100);
        let sql1 = "SELECT * FROM t WHERE id = 42";
        let stmt = axiomdb_sql::parser::parse(sql1, None).unwrap();
        cache.store(sql1, &stmt, 1);

        // Different structure: should miss.
        let sql2 = "SELECT * FROM t WHERE name = 'alice'";
        let result = cache.lookup(sql2, 1);
        assert!(result.is_none());
    }

    #[test]
    fn test_cache_invalidation_on_ddl() {
        let mut cache = PlanCache::new(100);
        let sql = "SELECT * FROM t WHERE id = 42";
        let stmt = axiomdb_sql::parser::parse(sql, None).unwrap();
        cache.store(sql, &stmt, 1);

        // Schema version changed → miss.
        let result = cache.lookup(sql, 2);
        assert!(result.is_none());
    }
}
