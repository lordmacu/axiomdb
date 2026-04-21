// ── ConstraintDef ─────────────────────────────────────────────────────────────

/// Kind of persisted catalog constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConstraintKind {
    Check = 0,
    Exclusion = 1,
}

impl TryFrom<u8> for ConstraintKind {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Check),
            1 => Ok(Self::Exclusion),
            _ => Err(DbError::ParseError {
                message: format!("unknown ConstraintKind byte: {value}"),
                position: None,
            }),
        }
    }
}

/// Operator used by one persisted exclusion-constraint element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConstraintOperator {
    Eq = 0,
    NotEq = 1,
    Lt = 2,
    LtEq = 3,
    Gt = 4,
    GtEq = 5,
    Overlaps = 6,
}

impl TryFrom<u8> for ConstraintOperator {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Eq),
            1 => Ok(Self::NotEq),
            2 => Ok(Self::Lt),
            3 => Ok(Self::LtEq),
            4 => Ok(Self::Gt),
            5 => Ok(Self::GtEq),
            6 => Ok(Self::Overlaps),
            _ => Err(DbError::ParseError {
                message: format!("unknown ConstraintOperator byte: {value}"),
                position: None,
            }),
        }
    }
}

/// One `(col_idx, operator)` pair owned by an exclusion constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusionElementDef {
    pub col_idx: u16,
    pub operator: ConstraintOperator,
}

/// A row in `axiom_constraints` — a named constraint persisted in the catalog.
///
/// CHECK constraints keep the legacy base layout. Exclusion constraints append a
/// trailer with the owned helper-index id and constrained column tuple.
///
/// ## Binary row format
///
/// ```text
/// [constraint_id: u32 LE][table_id: u32 LE]
/// [name_len: u32 LE][name: utf-8 bytes]
/// [expr_len: u32 LE][check_expr: utf-8 bytes]
/// [optional trailer]
///
/// trailer for exclusion constraints:
/// [kind: u8 = 1]
/// [owned_index_id: u32 LE]
/// [num_elements: u8]
/// repeated num_elements times:
///   [col_idx: u16 LE][operator: u8]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintDef {
    /// Catalog-allocated monotonic ID.
    pub constraint_id: u32,
    /// Table this constraint belongs to.
    pub table_id: u32,
    /// Constraint name (required — anonymous constraints not supported in ALTER TABLE).
    pub name: String,
    /// SQL expression string for CHECK constraints. Empty for exclusion rows.
    pub check_expr: String,
    /// Kind of constraint stored in this row.
    pub kind: ConstraintKind,
    /// `index_id` of the helper UNIQUE index owned by an exclusion constraint.
    /// `0` for CHECK constraints.
    pub owned_index_id: u32,
    /// Ordered exclusion tuple elements. Empty for CHECK constraints.
    pub exclude_elements: Vec<ExclusionElementDef>,
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
        if self.kind == ConstraintKind::Exclusion {
            buf.push(self.kind as u8);
            buf.extend_from_slice(&self.owned_index_id.to_le_bytes());
            buf.push(self.exclude_elements.len() as u8);
            for elem in &self.exclude_elements {
                buf.extend_from_slice(&elem.col_idx.to_le_bytes());
                buf.push(elem.operator as u8);
            }
        }
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
        let constraint_id = u32::from_le_bytes(data[0..4].try_into().unwrap_or_default());
        let table_id = u32::from_le_bytes(data[4..8].try_into().unwrap_or_default());

        let mut pos = 8usize;
        if data.len() < pos + 4 {
            return Err(DbError::ParseError {
                message: "ConstraintDef row truncated before name length".into(),
                position: None,
            });
        }
        let name_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or_default()) as usize;
        pos += 4;
        if data.len() < pos + name_len {
            return Err(DbError::ParseError {
                message: "ConstraintDef row truncated in name".into(),
                position: None,
            });
        }
        let name = String::from_utf8(data[pos..pos + name_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef name not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += name_len;

        if data.len() < pos + 4 {
            return Err(DbError::ParseError {
                message: "ConstraintDef row truncated before check_expr length".into(),
                position: None,
            });
        }
        let expr_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or_default()) as usize;
        pos += 4;
        if data.len() < pos + expr_len {
            return Err(DbError::ParseError {
                message: "ConstraintDef row truncated in check_expr".into(),
                position: None,
            });
        }
        let check_expr = String::from_utf8(data[pos..pos + expr_len].to_vec()).map_err(|e| {
            DbError::ParseError {
                message: format!("ConstraintDef check_expr not valid UTF-8: {e}"),
                position: None,
            }
        })?;
        pos += expr_len;

        if pos == data.len() {
            return Ok((
                Self {
                    constraint_id,
                    table_id,
                    name,
                    check_expr,
                    kind: ConstraintKind::Check,
                    owned_index_id: 0,
                    exclude_elements: Vec::new(),
                },
                pos,
            ));
        }

        let kind = ConstraintKind::try_from(data[pos])?;
        pos += 1;
        match kind {
            ConstraintKind::Check => {
                if pos != data.len() {
                    return Err(DbError::ParseError {
                        message: "unexpected trailing bytes in CHECK constraint row".into(),
                        position: None,
                    });
                }
                Ok((
                    Self {
                        constraint_id,
                        table_id,
                        name,
                        check_expr,
                        kind,
                        owned_index_id: 0,
                        exclude_elements: Vec::new(),
                    },
                    pos,
                ))
            }
            ConstraintKind::Exclusion => {
                if data.len() < pos + 5 {
                    return Err(DbError::ParseError {
                        message: "ConstraintDef exclusion trailer truncated".into(),
                        position: None,
                    });
                }
                let owned_index_id =
                    u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or_default());
                pos += 4;
                let num_elements = data[pos] as usize;
                pos += 1;
                let mut exclude_elements = Vec::with_capacity(num_elements);
                for _ in 0..num_elements {
                    if data.len() < pos + 3 {
                        return Err(DbError::ParseError {
                            message: "ConstraintDef exclusion element truncated".into(),
                            position: None,
                        });
                    }
                    let col_idx =
                        u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap_or_default());
                    pos += 2;
                    let operator = ConstraintOperator::try_from(data[pos])?;
                    pos += 1;
                    exclude_elements.push(ExclusionElementDef { col_idx, operator });
                }
                if pos != data.len() {
                    return Err(DbError::ParseError {
                        message: "unexpected trailing bytes in exclusion constraint row".into(),
                        position: None,
                    });
                }
                Ok((
                    Self {
                        constraint_id,
                        table_id,
                        name,
                        check_expr,
                        kind,
                        owned_index_id,
                        exclude_elements,
                    },
                    pos,
                ))
            }
        }
    }
}

#[cfg(test)]
mod constraint_tests {
    use super::*;

    #[test]
    fn test_constraint_def_check_roundtrip_legacy_shape() {
        let def = ConstraintDef {
            constraint_id: 7,
            table_id: 9,
            name: "ck_positive".into(),
            check_expr: "v > 0".into(),
            kind: ConstraintKind::Check,
            owned_index_id: 0,
            exclude_elements: vec![],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = ConstraintDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_constraint_def_exclusion_roundtrip() {
        let def = ConstraintDef {
            constraint_id: 11,
            table_id: 3,
            name: "users_slug_excl".into(),
            check_expr: String::new(),
            kind: ConstraintKind::Exclusion,
            owned_index_id: 42,
            exclude_elements: vec![
                ExclusionElementDef {
                    col_idx: 1,
                    operator: ConstraintOperator::Eq,
                },
                ExclusionElementDef {
                    col_idx: 3,
                    operator: ConstraintOperator::Eq,
                },
            ],
        };
        let bytes = def.to_bytes();
        let (back, consumed) = ConstraintDef::from_bytes(&bytes).unwrap();
        assert_eq!(back, def);
        assert_eq!(consumed, bytes.len());
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
/// Supports single-column and composite FKs (GAP-C.2). Legacy single-column
/// fields `child_col_idx` / `parent_col_idx` mirror the first element of
/// `child_col_idxs` / `parent_col_idxs` so older code paths keep working.
///
/// ## Binary row format
///
/// Legacy (single-column) layout — still written when `child_col_idxs.len() == 1`:
///
/// ```text
/// [fk_id:          4 bytes LE u32]
/// [child_table_id: 4 bytes LE u32]
/// [child_col_idx:  2 bytes LE u16]
/// [parent_table_id:4 bytes LE u32]
/// [parent_col_idx: 2 bytes LE u16]
/// [on_delete:      1 byte  u8   ]
/// [on_update:      1 byte  u8   ]
/// [fk_index_id:    4 bytes LE u32]
/// [name_len:       4 bytes LE u32]
/// [name:           name_len bytes UTF-8]
/// ```
///
/// Fixed header: 26 bytes.
///
/// Composite extension — only written when `child_col_idxs.len() > 1`:
/// appended after `name`:
///
/// ```text
/// [ext_magic:      1 byte  0xCF  ]
/// [num_pairs:      1 byte  u8    ] — total pair count (>= 2)
/// [extra_child_idxs: (num_pairs - 1) × u16 LE] — pairs beyond the first
/// [extra_parent_idxs:(num_pairs - 1) × u16 LE]
/// ```
///
/// Readers detect the extension by checking `data.len() > FIXED + name_len`.
/// Old rows (no extension) decode to single-column vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkDef {
    /// Catalog-allocated monotonic ID.
    pub fk_id: u32,
    /// Table that owns the FK column (the "child" / referencing table).
    pub child_table_id: u32,
    /// First child column index (legacy single-column field — always
    /// equals `child_col_idxs[0]`).
    pub child_col_idx: u16,
    /// Table being referenced (the "parent" table).
    pub parent_table_id: u32,
    /// First parent column index (legacy — always equals `parent_col_idxs[0]`).
    pub parent_col_idx: u16,
    /// Action when the parent row is deleted.
    pub on_delete: FkAction,
    /// Action when the parent key is updated.
    pub on_update: FkAction,
    /// `index_id` of the B-Tree index auto-created on the child FK columns.
    /// `0` means the user already had a suitable index — we did not create one.
    pub fk_index_id: u32,
    /// Constraint name.
    pub name: String,
    /// All child column indices in order (GAP-C.2). `len() == 1` for single-
    /// column FKs, `>= 2` for composite.
    pub child_col_idxs: Vec<u16>,
    /// All parent column indices in order. Must have the same length as
    /// `child_col_idxs` (parallel arrays).
    pub parent_col_idxs: Vec<u16>,
}

/// Marker byte introducing the composite-FK extension trailer.
const FK_COMPOSITE_EXT_MAGIC: u8 = 0xCF;

impl FkDef {
    /// Serializes this definition to bytes for heap storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let num_pairs = self.child_col_idxs.len();
        let ext_bytes = if num_pairs > 1 {
            2 + 4 * (num_pairs - 1)
        } else {
            0
        };
        let mut buf = Vec::with_capacity(26 + name_bytes.len() + ext_bytes);
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

        // Composite extension: only written for multi-column FKs.
        if num_pairs > 1 {
            buf.push(FK_COMPOSITE_EXT_MAGIC);
            buf.push(num_pairs as u8);
            for idx in self.child_col_idxs.iter().skip(1) {
                buf.extend_from_slice(&idx.to_le_bytes());
            }
            for idx in self.parent_col_idxs.iter().skip(1) {
                buf.extend_from_slice(&idx.to_le_bytes());
            }
        }
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

        let fk_id = u32::from_le_bytes(data[0..4].try_into().unwrap_or_default());
        let child_table_id = u32::from_le_bytes(data[4..8].try_into().unwrap_or_default());
        let child_col_idx = u16::from_le_bytes(data[8..10].try_into().unwrap_or_default());
        let parent_table_id = u32::from_le_bytes(data[10..14].try_into().unwrap_or_default());
        let parent_col_idx = u16::from_le_bytes(data[14..16].try_into().unwrap_or_default());
        let on_delete = FkAction::try_from(data[16])?;
        let on_update = FkAction::try_from(data[17])?;
        let fk_index_id = u32::from_le_bytes(data[18..22].try_into().unwrap_or_default());
        let name_len = u32::from_le_bytes(data[22..26].try_into().unwrap_or_default()) as usize;

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

        // Composite extension — appended after `name` when num_pairs > 1.
        let mut child_col_idxs = vec![child_col_idx];
        let mut parent_col_idxs = vec![parent_col_idx];
        let mut consumed = end;
        if data.len() > end && data[end] == FK_COMPOSITE_EXT_MAGIC {
            if data.len() < end + 2 {
                return Err(DbError::ParseError {
                    message: "FkDef composite extension truncated".into(),
                    position: None,
                });
            }
            let num_pairs = data[end + 1] as usize;
            if num_pairs < 2 {
                return Err(DbError::ParseError {
                    message: format!("FkDef composite num_pairs must be >= 2, got {num_pairs}"),
                    position: None,
                });
            }
            let extra = num_pairs - 1;
            let ext_data_end = end + 2 + 4 * extra;
            if data.len() < ext_data_end {
                return Err(DbError::ParseError {
                    message: "FkDef composite extension columns truncated".into(),
                    position: None,
                });
            }
            let mut off = end + 2;
            for _ in 0..extra {
                child_col_idxs.push(u16::from_le_bytes(
                    data[off..off + 2].try_into().unwrap_or_default(),
                ));
                off += 2;
            }
            for _ in 0..extra {
                parent_col_idxs.push(u16::from_le_bytes(
                    data[off..off + 2].try_into().unwrap_or_default(),
                ));
                off += 2;
            }
            consumed = ext_data_end;
        }

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
                child_col_idxs,
                parent_col_idxs,
            },
            consumed,
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
        let table_id = u32::from_le_bytes(data[0..4].try_into().unwrap_or_default());
        let col_idx = u16::from_le_bytes(data[4..6].try_into().unwrap_or_default());
        let row_count = u64::from_le_bytes(data[6..14].try_into().unwrap_or_default());
        let ndv = i64::from_le_bytes(data[14..22].try_into().unwrap_or_default());
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
