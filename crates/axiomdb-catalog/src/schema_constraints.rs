// ── ConstraintDef ─────────────────────────────────────────────────────────────

/// A row in `axiom_constraints` — a named constraint persisted in the catalog.
///
/// Currently used for CHECK constraints added via `ALTER TABLE ADD CONSTRAINT`.
/// UNIQUE constraints are stored as indexes in `axiom_indexes` instead.
///
/// ## Binary row format
///
/// ```text
/// [constraint_id: u32 LE][table_id: u32 LE]
/// [name_len: u32 LE][name: utf-8 bytes]
/// [expr_len: u32 LE][check_expr: utf-8 bytes]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintDef {
    /// Catalog-allocated monotonic ID.
    pub constraint_id: u32,
    /// Table this constraint belongs to.
    pub table_id: u32,
    /// Constraint name (required — anonymous constraints not supported in ALTER TABLE).
    pub name: String,
    /// SQL expression string for CHECK constraints. Empty for future types.
    pub check_expr: String,
}

impl ConstraintDef {
    /// Serializes this definition to bytes for heap storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let expr_bytes = self.check_expr.as_bytes();
        let mut buf = Vec::with_capacity(4 + 4 + 4 + name_bytes.len() + 4 + expr_bytes.len());
        buf.extend_from_slice(&self.constraint_id.to_le_bytes());
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&(expr_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(expr_bytes);
        buf
    }

    /// Deserializes a `ConstraintDef` from a byte slice.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        if data.len() < 8 {
            return Err(DbError::ParseError {
                message: "ConstraintDef row too short".into(),
                position: None,
            });
        }
        let constraint_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let table_id = u32::from_le_bytes(data[4..8].try_into().unwrap());

        let mut pos = 8usize;

        let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let name = String::from_utf8(data[pos..pos + name_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef name not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += name_len;

        let expr_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let check_expr = String::from_utf8(data[pos..pos + expr_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef check_expr not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += expr_len;

        Ok((
            Self {
                constraint_id,
                table_id,
                name,
                check_expr,
            },
            pos,
        ))
    }
}

// ── FkAction ──────────────────────────────────────────────────────────────────

/// The referential action taken when the parent row is deleted or updated.
///
/// Stored as a single byte in the `axiom_foreign_keys` heap.
/// `NoAction` and `Restrict` behave identically in AxiomDB (both enforce
/// immediately — deferred enforcement requires Phase 7 DEFERRABLE support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FkAction {
    /// Default SQL action — error if children exist (same as Restrict here).
    NoAction = 0,
    /// Error immediately if children exist.
    Restrict = 1,
    /// Delete / update child rows automatically.
    Cascade = 2,
    /// Set child FK column to NULL.
    SetNull = 3,
    /// Set child FK column to its DEFAULT value (deferred to Phase 6.9).
    SetDefault = 4,
}

impl TryFrom<u8> for FkAction {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NoAction),
            1 => Ok(Self::Restrict),
            2 => Ok(Self::Cascade),
            3 => Ok(Self::SetNull),
            4 => Ok(Self::SetDefault),
            _ => Err(DbError::ParseError {
                message: format!("unknown FkAction byte: {value}"),
                position: None,
            }),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for FkAction {
    fn default() -> Self {
        Self::NoAction
    }
}

// ── FkDef ─────────────────────────────────────────────────────────────────────

/// A row in `axiom_foreign_keys` — one entry per FK constraint (Phase 6.5).
///
/// Scoped to **single-column** FKs. Composite FK support is deferred to
/// Phase 6.9.
///
/// ## Binary row format
///
/// ```text
/// [fk_id:          4 bytes LE u32]
/// [child_table_id: 4 bytes LE u32]
/// [child_col_idx:  2 bytes LE u16]
/// [parent_table_id:4 bytes LE u32]
/// [parent_col_idx: 2 bytes LE u16]
/// [on_delete:      1 byte  u8   ]
/// [on_update:      1 byte  u8   ]
/// [fk_index_id:    4 bytes LE u32]  — auto-created index on child FK col;
///                                      0 = user-provided, not auto-created
/// [name_len:       4 bytes LE u32]
/// [name:           name_len bytes UTF-8]
/// ```
///
/// Fixed header: 26 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkDef {
    /// Catalog-allocated monotonic ID.
    pub fk_id: u32,
    /// Table that owns the FK column (the "child" / referencing table).
    pub child_table_id: u32,
    /// Column index in the child table that holds the FK value.
    pub child_col_idx: u16,
    /// Table being referenced (the "parent" table).
    pub parent_table_id: u32,
    /// Column index in the parent table that is referenced (must be PK/UNIQUE).
    pub parent_col_idx: u16,
    /// Action when the parent row is deleted.
    pub on_delete: FkAction,
    /// Action when the parent key is updated.
    pub on_update: FkAction,
    /// `index_id` of the B-Tree index auto-created on `child_col_idx`.
    /// `0` means the user already had a suitable index — we did not create one,
    /// and therefore we must NOT drop it when the FK is dropped.
    pub fk_index_id: u32,
    /// Constraint name. Auto-generated as `fk_{child_table}_{col}_{parent_table}`
    /// when not explicitly specified.
    pub name: String,
}

impl FkDef {
    /// Serializes this definition to bytes for heap storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let mut buf = Vec::with_capacity(26 + name_bytes.len());
        buf.extend_from_slice(&self.fk_id.to_le_bytes());
        buf.extend_from_slice(&self.child_table_id.to_le_bytes());
        buf.extend_from_slice(&self.child_col_idx.to_le_bytes());
        buf.extend_from_slice(&self.parent_table_id.to_le_bytes());
        buf.extend_from_slice(&self.parent_col_idx.to_le_bytes());
        buf.push(self.on_delete as u8);
        buf.push(self.on_update as u8);
        buf.extend_from_slice(&self.fk_index_id.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf
    }

    /// Deserializes a `FkDef` from a byte slice.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        const FIXED: usize = 26;
        if data.len() < FIXED {
            return Err(DbError::ParseError {
                message: format!(
                    "FkDef row too short: need {FIXED} bytes, got {}",
                    data.len()
                ),
                position: None,
            });
        }

        let fk_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let child_table_id = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let child_col_idx = u16::from_le_bytes(data[8..10].try_into().unwrap());
        let parent_table_id = u32::from_le_bytes(data[10..14].try_into().unwrap());
        let parent_col_idx = u16::from_le_bytes(data[14..16].try_into().unwrap());
        let on_delete = FkAction::try_from(data[16])?;
        let on_update = FkAction::try_from(data[17])?;
        let fk_index_id = u32::from_le_bytes(data[18..22].try_into().unwrap());
        let name_len = u32::from_le_bytes(data[22..26].try_into().unwrap()) as usize;

        let end = FIXED + name_len;
        if data.len() < end {
            return Err(DbError::ParseError {
                message: format!(
                    "FkDef row truncated: name claims {name_len} bytes but only {} remain",
                    data.len() - FIXED
                ),
                position: None,
            });
        }
        let name =
            String::from_utf8(data[FIXED..end].to_vec()).map_err(|e| DbError::ParseError {
                message: format!("FkDef name not valid UTF-8: {e}"),
                position: None,
            })?;

        Ok((
            Self {
                fk_id,
                child_table_id,
                child_col_idx,
                parent_table_id,
                parent_col_idx,
                on_delete,
                on_update,
                fk_index_id,
                name,
            },
            end,
        ))
    }
}

// ── StatsDef ──────────────────────────────────────────────────────────────────

/// Per-column statistics stored in `axiom_stats` (Phase 6.10).
///
/// Used by the query planner to choose between index scan and full table scan
/// via selectivity estimation: `selectivity = 1.0 / ndv`.
///
/// ## Binary format (22 bytes fixed)
///
/// ```text
/// [table_id:  4 bytes LE u32]
/// [col_idx:   2 bytes LE u16]
/// [row_count: 8 bytes LE u64]  — visible rows at last ANALYZE / CREATE INDEX
/// [ndv:       8 bytes LE i64]  — distinct non-NULL values (PostgreSQL dual-encoding)
/// ```
///
/// `ndv` encoding (same as PostgreSQL `stadistinct`):
/// - `> 0` → absolute distinct count (e.g. 9999 unique emails)
/// - `< 0` → proportion multiplier (reserved; Phase 6.10 always writes > 0)
/// - `= 0` → unknown → planner uses `DEFAULT_NUM_DISTINCT = 200`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsDef {
    pub table_id: u32,
    pub col_idx: u16,
    pub row_count: u64,
    pub ndv: i64,
}

impl StatsDef {
    /// Serializes to the 22-byte binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(22);
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.col_idx.to_le_bytes());
        buf.extend_from_slice(&self.row_count.to_le_bytes());
        buf.extend_from_slice(&self.ndv.to_le_bytes());
        buf
    }

    /// Deserializes from the 22-byte binary format.
    ///
    /// Returns `(def, bytes_consumed)`.
    pub fn from_bytes(data: &[u8]) -> Result<(Self, usize), DbError> {
        if data.len() < 22 {
            return Err(DbError::ParseError {
                message: format!("StatsDef row too short: need 22 bytes, got {}", data.len()),
                position: None,
            });
        }
        let table_id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let col_idx = u16::from_le_bytes(data[4..6].try_into().unwrap());
        let row_count = u64::from_le_bytes(data[6..14].try_into().unwrap());
        let ndv = i64::from_le_bytes(data[14..22].try_into().unwrap());
        Ok((
            Self {
                table_id,
                col_idx,
                row_count,
                ndv,
            },
            22,
        ))
    }
}
