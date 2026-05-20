// ── TableDef ──────────────────────────────────────────────────────────────────

/// Metadata for a user table — one row in `axiom_tables`.
///
/// ## On-disk format (`to_bytes` / `from_bytes`)
///
/// ```text
/// legacy (v0):
///   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8]
///
/// v1:
///   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8][layout:1]
///
/// v2 (current):
///   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8][layout:1][schema_version:8 LE]
/// ```
///
/// v0 rows decode as `storage_layout = Heap, schema_version = 1`.
/// v1 rows decode as `schema_version = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TablePersistence {
    #[default]
    Permanent,
    Temporary,
    Unlogged,
}

impl From<TablePersistence> for u8 {
    fn from(value: TablePersistence) -> Self {
        match value {
            TablePersistence::Permanent => 0,
            TablePersistence::Temporary => 1,
            TablePersistence::Unlogged => 2,
        }
    }
}

impl TryFrom<u8> for TablePersistence {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Permanent),
            1 => Ok(Self::Temporary),
            2 => Ok(Self::Unlogged),
            other => Err(DbError::ParseError {
                message: format!("invalid table persistence byte {other}"),
                position: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelationKind {
    #[default]
    Table,
    MaterializedView,
    View,
}

impl From<RelationKind> for u8 {
    fn from(value: RelationKind) -> Self {
        match value {
            RelationKind::Table => 0,
            RelationKind::MaterializedView => 1,
            RelationKind::View => 2,
        }
    }
}

impl TryFrom<u8> for RelationKind {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Table),
            1 => Ok(Self::MaterializedView),
            2 => Ok(Self::View),
            other => Err(DbError::ParseError {
                message: format!("invalid relation kind byte {other}"),
                position: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

impl From<TriggerEvent> for u8 {
    fn from(value: TriggerEvent) -> Self {
        match value {
            TriggerEvent::Insert => 1,
            TriggerEvent::Update => 2,
            TriggerEvent::Delete => 3,
        }
    }
}

impl TryFrom<u8> for TriggerEvent {
    type Error = DbError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Insert),
            2 => Ok(Self::Update),
            3 => Ok(Self::Delete),
            other => Err(DbError::ParseError {
                message: format!("invalid trigger event byte {other}"),
                position: None,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerDef {
    pub name: String,
    pub event: TriggerEvent,
    pub body_sql: String,
    pub ordinal: u32,
}

impl TriggerDef {
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let body = self.body_sql.as_bytes();
        debug_assert!(name.len() <= 255, "trigger name too long");
        debug_assert!(body.len() <= u16::MAX as usize, "trigger body too long");
        let mut buf = Vec::with_capacity(1 + name.len() + 1 + 2 + body.len() + 4);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(self.event.into());
        buf.extend_from_slice(&(body.len() as u16).to_le_bytes());
        buf.extend_from_slice(body);
        buf.extend_from_slice(&self.ordinal.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated TriggerDef bytes".into(),
            position: None,
        };
        if bytes.is_empty() {
            return Err(err());
        }
        let name_len = bytes[0] as usize;
        if bytes.len() < 1 + name_len + 1 + 2 + 4 {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[1..1 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in trigger name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = 1 + name_len;
        let event = TriggerEvent::try_from(bytes[consumed])?;
        consumed += 1;
        let body_len = u16::from_le_bytes(
            bytes[consumed..consumed + 2]
                .try_into()
                .expect("slice is exactly 2 bytes"),
        ) as usize;
        consumed += 2;
        if bytes.len() < consumed + body_len + 4 {
            return Err(err());
        }
        let body_sql = std::str::from_utf8(&bytes[consumed..consumed + body_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in trigger body".into(),
                position: None,
            })?
            .to_string();
        consumed += body_len;
        let ordinal = u32::from_le_bytes(
            bytes[consumed..consumed + 4]
                .try_into()
                .expect("slice is exactly 4 bytes"),
        );
        consumed += 4;
        Ok((
            Self {
                name,
                event,
                body_sql,
                ordinal,
            },
            consumed,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub id: TableId,
    /// Root page of the table's primary row store.
    ///
    /// For heap tables this points to the `HeapChain` root.
    /// For clustered tables this points to the clustered leaf/internal root.
    /// Must never be 0 (page 0 is the meta page).
    pub root_page_id: u64,
    pub storage_layout: TableStorageLayout,
    pub schema_name: String,
    pub table_name: String,
    /// Monotonically increasing version counter for this table's schema.
    ///
    /// Initialized to `1` when the table is created. Bumped by every DDL
    /// operation that modifies this specific table (CREATE INDEX, DROP INDEX,
    /// DROP TABLE, TRUNCATE TABLE). Used by the plan cache to detect stale
    /// cached plans without invalidating the entire cache on any DDL event.
    ///
    /// Mirrors PostgreSQL's per-relation version tracking in `relcache`.
    pub schema_version: u64,
    /// `true` when the table was declared `IMMUTABLE` (Phase 13.9). The
    /// executor rejects every UPDATE and DELETE statement that would modify
    /// an immutable table; INSERTs still succeed. Stored in the v3 on-disk
    /// row format; v0–v2 rows decode as `false`.
    pub immutable: bool,
    /// Persistence class of the table. Stored in the v4 on-disk row format;
    /// v0–v3 rows decode as permanent.
    pub persistence: TablePersistence,
    /// Relation kind. Stored in the v5 on-disk row format; older rows decode
    /// as ordinary tables.
    pub relation_kind: RelationKind,
    /// Defining query for materialized views. `None` for ordinary tables.
    pub defining_query: Option<String>,
    /// Default collation declared on the table. `None` = inherit from
    /// database/session defaults.
    pub default_collation: Option<String>,
    /// Statement-level triggers owned by this table.
    pub triggers: Vec<TriggerDef>,
}

impl TableDef {
    pub fn is_heap(&self) -> bool {
        self.storage_layout == TableStorageLayout::Heap
    }

    pub fn is_clustered(&self) -> bool {
        self.storage_layout == TableStorageLayout::Clustered
    }

    pub fn is_materialized_view(&self) -> bool {
        self.relation_kind == RelationKind::MaterializedView
    }

    pub fn is_view(&self) -> bool {
        self.relation_kind == RelationKind::View
    }

    pub fn ensure_heap_runtime(&self, feature: &str) -> Result<(), DbError> {
        if self.is_clustered() {
            return Err(DbError::NotImplemented {
                feature: feature.to_string(),
            });
        }
        Ok(())
    }

    /// Serializes to binary row format (v7).
    ///
    /// Format:
    /// `[table_id:4][root_page_id:8][schema_len:1][schema bytes][name_len:1][name bytes][layout:1][schema_version:8 LE]`
    ///
    /// # Panics (debug only)
    /// If `schema_name` or `table_name` exceeds 255 bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let schema = self.schema_name.as_bytes();
        let name = self.table_name.as_bytes();
        let query = self.defining_query.as_deref().unwrap_or("").as_bytes();
        let collation = self.default_collation.as_deref().unwrap_or("").as_bytes();
        let trigger_bytes: Vec<Vec<u8>> = self.triggers.iter().map(TriggerDef::to_bytes).collect();
        let triggers_len: usize = trigger_bytes.iter().map(Vec::len).sum();
        debug_assert!(schema.len() <= 255, "schema_name too long");
        debug_assert!(name.len() <= 255, "table_name too long");
        debug_assert!(query.len() <= u16::MAX as usize, "defining_query too long");
        debug_assert!(collation.len() <= u16::MAX as usize, "default_collation too long");
        debug_assert!(self.triggers.len() <= u16::MAX as usize, "too many triggers");

        let mut buf = Vec::with_capacity(
            4 + 8 + 1 + schema.len() + 1 + name.len() + 1 + 8 + 1 + 1 + 1 + 2 + query.len() + 2
                + collation.len()
                + 2
                + triggers_len,
        );
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.extend_from_slice(&self.root_page_id.to_le_bytes());
        buf.push(schema.len() as u8);
        buf.extend_from_slice(schema);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf.push(self.storage_layout.into());
        buf.extend_from_slice(&self.schema_version.to_le_bytes());
        // v3 trailer: 1-byte `immutable` flag (Phase 13.9). Older readers
        // treat the extra byte as unexpected trailing data; newer readers
        // accept either 9 (v2) or 10 (v3) trailing bytes.
        buf.push(self.immutable as u8);
        // v4 trailer: 1-byte persistence flag (Phase 21.7). Older rows decode
        // as permanent.
        buf.push(self.persistence.into());
        // v5 trailer: relation kind + defining query length + defining query bytes.
        buf.push(self.relation_kind.into());
        buf.extend_from_slice(&(query.len() as u16).to_le_bytes());
        buf.extend_from_slice(query);
        // v7 trailer: default collation + trigger count + trigger bytes.
        buf.extend_from_slice(&(collation.len() as u16).to_le_bytes());
        buf.extend_from_slice(collation);
        buf.extend_from_slice(&(self.triggers.len() as u16).to_le_bytes());
        for trig in trigger_bytes {
            buf.extend_from_slice(&trig);
        }
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Returns `(TableDef, bytes_consumed)` on success.
    ///
    /// # Errors
    /// - [`DbError::ParseError`] if `bytes` is too short or contains invalid UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated TableRow bytes".into(),
            position: None,
        };

        // Minimum: 4 (id) + 8 (root_page_id) + 1 (schema_len) + 0 + 1 (name_len) + 0 = 14
        if bytes.len() < 14 {
            return Err(err());
        }

        let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let root_page_id = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        let schema_len = bytes[12] as usize;
        let pos = 13;

        if bytes.len() < pos + schema_len + 1 {
            return Err(err());
        }
        let schema_name = std::str::from_utf8(&bytes[pos..pos + schema_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in schema_name".into(),
                position: None,
            })?
            .to_string();
        let pos = pos + schema_len;

        let name_len = bytes[pos] as usize;
        let pos = pos + 1;
        if bytes.len() < pos + name_len {
            return Err(err());
        }
        let table_name = std::str::from_utf8(&bytes[pos..pos + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in table_name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = pos + name_len;
        // On-disk formats — decode trailing bytes by total remaining size:
        //   v0 (0 trailing):  storage_layout = Heap, schema_version = 1, immutable = false, persistence = Permanent
        //   v1 (1 trailing):  storage_layout from byte, schema_version = 1, immutable = false, persistence = Permanent
        //   v2 (9 trailing):  storage_layout + schema_version, immutable = false, persistence = Permanent
        //   v3 (10 trailing): v2 + 1-byte immutable flag (Phase 13.9), persistence = Permanent
        //   v4 (11 trailing): v3 + 1-byte persistence flag (Phase 21.7)
        //   v5 (>=14 trailing): v4 + relation kind + defining query bytes
        //   v6 (v5 + trailing bytes after query): trigger count + trigger bytes
        //   v7 (v5 + collation_len + collation bytes + trigger count + trigger bytes)
        let remaining = bytes.len() - consumed;
        let (
            storage_layout,
            schema_version,
            immutable,
            persistence,
            relation_kind,
            defining_query,
            default_collation,
            triggers,
        ) = match remaining {
            0 => (
                TableStorageLayout::Heap,
                1u64,
                false,
                TablePersistence::Permanent,
                RelationKind::Table,
                None,
                None,
                Vec::new(),
            ),
            1 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                (
                    layout,
                    1u64,
                    false,
                    TablePersistence::Permanent,
                    RelationKind::Table,
                    None,
                    None,
                    Vec::new(),
                )
            }
            9 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                let v = u64::from_le_bytes(
                    bytes[consumed..consumed + 8]
                        .try_into()
                        .expect("slice is exactly 8 bytes"),
                );
                consumed += 8;
                (
                    layout,
                    v,
                    false,
                    TablePersistence::Permanent,
                    RelationKind::Table,
                    None,
                    None,
                    Vec::new(),
                )
            }
            10 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                let v = u64::from_le_bytes(
                    bytes[consumed..consumed + 8]
                        .try_into()
                        .expect("slice is exactly 8 bytes"),
                );
                consumed += 8;
                let imm = bytes[consumed] != 0;
                consumed += 1;
                (
                    layout,
                    v,
                    imm,
                    TablePersistence::Permanent,
                    RelationKind::Table,
                    None,
                    None,
                    Vec::new(),
                )
            }
            11 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                let v = u64::from_le_bytes(
                    bytes[consumed..consumed + 8]
                        .try_into()
                        .expect("slice is exactly 8 bytes"),
                );
                consumed += 8;
                let imm = bytes[consumed] != 0;
                consumed += 1;
                let persistence = TablePersistence::try_from(bytes[consumed])?;
                consumed += 1;
                (
                    layout,
                    v,
                    imm,
                    persistence,
                    RelationKind::Table,
                    None,
                    None,
                    Vec::new(),
                )
            }
            n if n >= 14 => {
                let layout = TableStorageLayout::try_from(bytes[consumed])?;
                consumed += 1;
                let v = u64::from_le_bytes(
                    bytes[consumed..consumed + 8]
                        .try_into()
                        .expect("slice is exactly 8 bytes"),
                );
                consumed += 8;
                let imm = bytes[consumed] != 0;
                consumed += 1;
                let persistence = TablePersistence::try_from(bytes[consumed])?;
                consumed += 1;
                let relation_kind = RelationKind::try_from(bytes[consumed])?;
                consumed += 1;
                let query_len = u16::from_le_bytes(
                    bytes[consumed..consumed + 2]
                        .try_into()
                        .expect("slice is exactly 2 bytes"),
                ) as usize;
                consumed += 2;
                if bytes.len() < consumed + query_len {
                    return Err(err());
                }
                let defining_query = if query_len == 0 {
                    None
                } else {
                    Some(
                        std::str::from_utf8(&bytes[consumed..consumed + query_len])
                            .map_err(|_| DbError::ParseError {
                                message: "invalid UTF-8 in defining_query".into(),
                                position: None,
                            })?
                            .to_string(),
                    )
                };
                consumed += query_len;
                let (default_collation, triggers) = if bytes.len() > consumed {
                    if bytes.len() < consumed + 2 {
                        return Err(err());
                    }
                    let first_u16 = u16::from_le_bytes(
                        bytes[consumed..consumed + 2]
                            .try_into()
                            .expect("slice is exactly 2 bytes"),
                    ) as usize;
                    if bytes.len() >= consumed + 2 + first_u16 + 2 {
                        let coll_start = consumed + 2;
                        let coll_end = coll_start + first_u16;
                        let collation = if first_u16 == 0 {
                            None
                        } else {
                            Some(
                                std::str::from_utf8(&bytes[coll_start..coll_end])
                                    .map_err(|_| DbError::ParseError {
                                        message: "invalid UTF-8 in default_collation".into(),
                                        position: None,
                                    })?
                                    .to_string(),
                            )
                        };
                        consumed = coll_end;
                        let trigger_count = u16::from_le_bytes(
                            bytes[consumed..consumed + 2]
                                .try_into()
                                .expect("slice is exactly 2 bytes"),
                        ) as usize;
                        consumed += 2;
                        let mut triggers = Vec::with_capacity(trigger_count);
                        for _ in 0..trigger_count {
                            let (trigger, used) = TriggerDef::from_bytes(&bytes[consumed..])?;
                            consumed += used;
                            triggers.push(trigger);
                        }
                        (collation, triggers)
                    } else {
                        let trigger_count = first_u16;
                        consumed += 2;
                        let mut triggers = Vec::with_capacity(trigger_count);
                        for _ in 0..trigger_count {
                            let (trigger, used) = TriggerDef::from_bytes(&bytes[consumed..])?;
                            consumed += used;
                            triggers.push(trigger);
                        }
                        (None, triggers)
                    }
                } else {
                    (None, Vec::new())
                };
                (
                    layout,
                    v,
                    imm,
                    persistence,
                    relation_kind,
                    defining_query,
                    default_collation,
                    triggers,
                )
            }
            _ => {
                return Err(DbError::ParseError {
                    message: "unexpected trailing bytes in TableRow".into(),
                    position: None,
                })
            }
        };

        Ok((
            Self {
                id,
                root_page_id,
                storage_layout,
                schema_name,
                table_name,
                schema_version,
                immutable,
                persistence,
                relation_kind,
                defining_query,
                default_collation,
                triggers,
            },
            consumed,
        ))
    }
}

// ── IdentityKind ──────────────────────────────────────────────────────────────

/// SQL:2003 identity column kind (Phase 24.1c).
///
/// - `None`: regular column or MySQL-style `AUTO_INCREMENT`.
/// - `Always`: `GENERATED ALWAYS AS IDENTITY` — explicit INSERT values rejected.
/// - `ByDefault`: `GENERATED BY DEFAULT AS IDENTITY` — explicit values accepted,
///   NULL still auto-generates.
///
/// Stored as a trailing byte in `ColumnDef::to_bytes` only when non-`None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentityKind {
    #[default]
    None = 0,
    Always = 1,
    ByDefault = 2,
}

impl From<IdentityKind> for u8 {
    fn from(k: IdentityKind) -> u8 {
        k as u8
    }
}

impl TryFrom<u8> for IdentityKind {
    type Error = ();
    fn try_from(b: u8) -> Result<Self, ()> {
        match b {
            1 => Ok(Self::Always),
            2 => Ok(Self::ByDefault),
            _ => Ok(Self::None),
        }
    }
}

// ── ColumnDef ─────────────────────────────────────────────────────────────────

/// Metadata for a single column — one row in `axiom_columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub table_id: TableId,
    pub col_idx: u16,
    pub name: String,
    pub col_type: ColumnType,
    pub nullable: bool,
    /// `true` if this column was declared `AUTO_INCREMENT` or `SERIAL`.
    pub auto_increment: bool,
    /// Maximum character length for `VARCHAR(N)` / `CHAR(N)` columns.
    /// `0` means unbounded (equivalent to `TEXT`). Stored as 2 bytes after
    /// the name when `flags bit2` is set (backward-compatible extension).
    pub type_len: u16,
    /// `true` when the column was declared `CHAR(N)` (fixed-length).
    /// `false` for `VARCHAR(N)` and `TEXT`.
    /// CHAR values are right-padded with spaces to `type_len` on write
    /// and trailing spaces are stripped on read (MySQL default behavior).
    /// Stored in `flags bit3` (backward-compatible: old rows read as `false`).
    pub is_fixed_len: bool,
    /// SQL text of the DEFAULT expression (e.g. `"42"`, `"'active'"`, `"-1"`).
    /// `None` means no DEFAULT was declared (missing columns default to NULL).
    /// Stored as `[u16 len][utf8 bytes]` after the other extensions when
    /// `flags bit4` is set (backward-compatible: old rows read as `None`).
    pub default_expr: Option<String>,
    /// SQL text of the `ON UPDATE` expression (typically `CURRENT_TIMESTAMP`).
    /// When set, the executor auto-evaluates it on every UPDATE unless the
    /// column is explicitly assigned. Ubiquitous in MySQL audit patterns
    /// (`updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`). Stored right
    /// after `default_expr` when `flags bit5` is set — old rows read as
    /// `None`.
    pub on_update_expr: Option<String>,
    /// SQL text of `GENERATED ALWAYS AS (expr)` for generated columns.
    /// `None` means this is a normal base column. Stored after
    /// `on_update_expr` when `flags bit6` is set — old rows read as `None`.
    pub generated_expr: Option<String>,
    /// Explicit column collation. `None` means inherit table/database default.
    /// Stored as an optional trailing `[u16 len][utf8 bytes]` after
    /// `generated_expr` — old rows stop earlier and read as `None`.
    pub collation: Option<String>,
    /// `true` for STORED generated columns, `false` for VIRTUAL generated
    /// columns. Only meaningful when `generated_expr` is `Some`.
    /// Stored in `flags bit7` as inverted virtual marker: 0 = STORED,
    /// 1 = VIRTUAL.
    pub generated_stored: bool,
    /// Fully-qualified declared enum type name (`schema.type`) for text-backed
    /// enum columns. `None` means the physical column type is the declared SQL
    /// type. Stored as a trailing optional `[u16 len][utf8 bytes]` after
    /// collation; old rows stop earlier and read as `None`.
    pub enum_type_name: Option<String>,
    /// Array element column type (Phase 20.4). `Some(ct)` when `col_type` is
    /// `ColumnType::Array`. `None` for non-array columns. Stored as trailing
    /// bytes after `enum_type_name`.
    pub array_element_type: Option<ColumnType>,
    /// Number of array dimensions (1-6). `Some(ndims)` when the column is an
    /// array type. `None` for non-array columns. Stored as trailing bytes
    /// after `enum_type_name`.
    pub array_ndims: Option<u8>,
    /// SQL:2003 identity column kind (Phase 24.1c). `None` for regular and
    /// MySQL `AUTO_INCREMENT` columns; `Always` / `ByDefault` for the
    /// SQL-standard `GENERATED ALWAYS|BY DEFAULT AS IDENTITY` form.
    /// Stored as a trailing byte after the array fields, only when non-`None`.
    pub identity_kind: IdentityKind,
}

impl ColumnDef {
    /// Serializes to binary row format.
    ///
    /// Format: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
    /// - `flags bit0` = nullable
    /// - `flags bit1` = auto_increment
    /// - `flags bit2` = type_len extension present
    /// - `flags bit3` = is_fixed_len (CHAR vs VARCHAR)
    /// - `flags bit4` = default_expr extension present
    /// - `flags bit5` = on_update_expr extension present
    /// - `flags bit6` = generated_expr extension present
    /// - `flags bit7` = generated kind: 0 STORED, 1 VIRTUAL
    ///
    /// Trailing extension fields are stored after the flagged payloads:
    /// `[collation_len:u16][collation]` when collation or enum metadata exists,
    /// then `[enum_type_len:u16][enum_type]` when enum metadata exists.
    /// Then `[array_ndims:u8]` when `array_ndims` is > 0 (Phase 20.4),
    /// followed by `[array_element_type:u8]` when ndims > 0.
    pub fn to_bytes(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        debug_assert!(name.len() <= 255, "column name too long");

        let has_type_len = self.type_len > 0;
        let default_bytes = self.default_expr.as_deref().map(|s| s.as_bytes());
        let has_default = default_bytes.is_some();
        let on_update_bytes = self.on_update_expr.as_deref().map(|s| s.as_bytes());
        let has_on_update = on_update_bytes.is_some();
        let generated_bytes = self.generated_expr.as_deref().map(|s| s.as_bytes());
        let has_generated = generated_bytes.is_some();
        // When enum_type_name is present we must always emit a collation section (even
        // an empty one) so the decoder can unambiguously find the enum field. Without
        // this invariant a `collation=None, enum_type_name=Some(...)` row would write
        // the enum length bytes at the position the decoder expects collation, causing
        // silent data corruption (the enum string is misread as the collation value).
        let collation_bytes: Option<&[u8]> = if self.enum_type_name.is_some() {
            Some(self.collation.as_deref().unwrap_or("").as_bytes())
        } else {
            self.collation.as_deref().map(|s| s.as_bytes())
        };
        let enum_type_bytes = self.enum_type_name.as_deref().map(|s| s.as_bytes());

        let mut flags: u8 = 0;
        if self.nullable {
            flags |= 0x01;
        }
        if self.auto_increment {
            flags |= 0x02;
        }
        if has_type_len {
            flags |= 0x04; // bit2: type_len extension present
        }
        if self.is_fixed_len {
            flags |= 0x08; // bit3: CHAR (fixed-length)
        }
        if has_default {
            flags |= 0x10; // bit4: default_expr present
        }
        if has_on_update {
            flags |= 0x20; // bit5: on_update_expr present
        }
        if has_generated {
            flags |= 0x40; // bit6: generated_expr present
            if !self.generated_stored {
                flags |= 0x80; // bit7: virtual generated column
            }
        }
        let extra = if has_type_len { 2 } else { 0 }
            + default_bytes.map_or(0, |b| 2 + b.len())
            + on_update_bytes.map_or(0, |b| 2 + b.len())
            + generated_bytes.map_or(0, |b| 2 + b.len())
            // Collation: 2-byte length prefix + data (only when present)
            + collation_bytes.map_or(0, |b| 2 + b.len())
            // Enum_type_name: 2-byte length prefix + data (only when present)
            + enum_type_bytes.map_or(0, |b| 2 + b.len())
            + if self.array_ndims.unwrap_or(0) > 0 {
                2 // 1 byte ndims + 1 byte element_type
            } else {
                0
            };
        let mut buf = Vec::with_capacity(4 + 2 + 1 + 1 + 1 + name.len() + extra);
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        buf.extend_from_slice(&self.col_idx.to_le_bytes());
        buf.push(u8::from(self.col_type));
        buf.push(flags);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        if has_type_len {
            buf.extend_from_slice(&self.type_len.to_le_bytes());
        }
        if let Some(db) = default_bytes {
            buf.extend_from_slice(&(db.len() as u16).to_le_bytes());
            buf.extend_from_slice(db);
        }
        if let Some(ob) = on_update_bytes {
            buf.extend_from_slice(&(ob.len() as u16).to_le_bytes());
            buf.extend_from_slice(ob);
        }
        if let Some(gb) = generated_bytes {
            buf.extend_from_slice(&(gb.len() as u16).to_le_bytes());
            buf.extend_from_slice(gb);
        }
        // Write collation only when present (original behavior)
        if let Some(cb) = collation_bytes {
            buf.extend_from_slice(&(cb.len() as u16).to_le_bytes());
            buf.extend_from_slice(cb);
        }
        // Write enum_type_name only when present
        if let Some(eb) = enum_type_bytes {
            buf.extend_from_slice(&(eb.len() as u16).to_le_bytes());
            buf.extend_from_slice(eb);
        }
        // Phase 20.4: array trailing fields
        if self.array_ndims.unwrap_or(0) > 0 {
            buf.push(self.array_ndims.unwrap_or(0));
            buf.push(
                self.array_element_type
                    .map(u8::from)
                    .unwrap_or(0),
            );
        }
        // Phase 24.1c: identity kind trailing byte, only when non-`None`.
        // Old readers stop before this byte; new readers fall back to `None`
        // when the byte is absent (see `from_bytes`).
        if self.identity_kind != IdentityKind::None {
            buf.push(self.identity_kind as u8);
        }
        buf
    }

    /// Deserializes from binary row format.
    ///
    /// Backward-compatible: bit1 of flags was always 0 in older rows,
    /// so `auto_increment` defaults to `false` for pre-4.14 catalog data.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> {
        let err = || DbError::ParseError {
            message: "truncated ColumnRow bytes".into(),
            position: None,
        };

        if bytes.len() < 9 {
            return Err(err());
        }

        let table_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let col_idx = u16::from_le_bytes([bytes[4], bytes[5]]);
        let col_type = ColumnType::try_from(bytes[6])?;
        let flags = bytes[7];
        let nullable = flags & 0x01 != 0;
        let auto_increment = flags & 0x02 != 0;
        let has_type_len = flags & 0x04 != 0;
        let is_fixed_len = flags & 0x08 != 0;
        let has_default = flags & 0x10 != 0;
        let has_on_update = flags & 0x20 != 0;
        let has_generated = flags & 0x40 != 0;
        let generated_stored = has_generated && flags & 0x80 == 0;
        let name_len = bytes[8] as usize;

        if bytes.len() < 9 + name_len {
            return Err(err());
        }
        let name = std::str::from_utf8(&bytes[9..9 + name_len])
            .map_err(|_| DbError::ParseError {
                message: "invalid UTF-8 in column name".into(),
                position: None,
            })?
            .to_string();
        let mut consumed = 9 + name_len;

        // Backward-compatible extension: type_len present only when bit2 set.
        let type_len = if has_type_len {
            if bytes.len() < consumed + 2 {
                return Err(err());
            }
            let v = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]);
            consumed += 2;
            v
        } else {
            0
        };

        // Backward-compatible extension: default_expr present only when bit4 set.
        let default_expr = if has_default {
            if bytes.len() < consumed + 2 {
                return Err(err());
            }
            let dlen = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if bytes.len() < consumed + dlen {
                return Err(err());
            }
            let s = std::str::from_utf8(&bytes[consumed..consumed + dlen])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in default_expr".into(),
                    position: None,
                })?
                .to_string();
            consumed += dlen;
            Some(s)
        } else {
            None
        };

        // on_update_expr present only when bit5 set (old rows read as None).
        let on_update_expr = if has_on_update {
            if bytes.len() < consumed + 2 {
                return Err(err());
            }
            let olen = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if bytes.len() < consumed + olen {
                return Err(err());
            }
            let s = std::str::from_utf8(&bytes[consumed..consumed + olen])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in on_update_expr".into(),
                    position: None,
                })?
                .to_string();
            consumed += olen;
            Some(s)
        } else {
            None
        };

        // generated_expr present only when bit6 set (old rows read as None).
        let generated_expr = if has_generated {
            if bytes.len() < consumed + 2 {
                return Err(err());
            }
            let glen = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if bytes.len() < consumed + glen {
                return Err(err());
            }
            let s = std::str::from_utf8(&bytes[consumed..consumed + glen])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in generated_expr".into(),
                    position: None,
                })?
                .to_string();
            consumed += glen;
            Some(s)
        } else {
            None
        };

        let mut enum_type_name = None;
        // Collation section: written only when collation_bytes.is_some() in to_bytes.
        // We only enter this block if there are at least 4 bytes: 2 for the length prefix
        // plus 2 minimum for the data itself. This prevents misinterpreting array metadata
        // (ndims + elem_type = 2 bytes) as a collation section.
        let collation = if bytes.len() >= consumed + 4 {
            if bytes.len() < consumed + 2 {
                return Err(err());
            }
            let clen = u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
            consumed += 2;
            if bytes.len() < consumed + clen {
                return Err(err());
            }
            let s = std::str::from_utf8(&bytes[consumed..consumed + clen])
                .map_err(|_| DbError::ParseError {
                    message: "invalid UTF-8 in column collation".into(),
                    position: None,
                })?
                .to_string();
            consumed += clen;
            // Enum_type_name section: written only when enum_type_bytes.is_some() in to_bytes.
            // Distinguishing "no enum" from "enum empty": if exactly 4 bytes remain after
            // collation, it's [collation_len=0][ndims][elem_type] (no enum). If >= 5 bytes
            // remain, the first 2 bytes are the enum length prefix.
            if bytes.len() >= consumed + 5 {
                let elen =
                    u16::from_le_bytes([bytes[consumed], bytes[consumed + 1]]) as usize;
                consumed += 2;
                if bytes.len() < consumed + elen {
                    return Err(err());
                }
                enum_type_name = Some(
                    std::str::from_utf8(&bytes[consumed..consumed + elen])
                        .map_err(|_| DbError::ParseError {
                            message: "invalid UTF-8 in column enum type".into(),
                            position: None,
                        })?
                        .to_string(),
                );
                consumed += elen;
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            } else if s.is_empty() {
                None
            } else {
                Some(s)
            }
        } else {
            None
        };

        // Phase 20.4: array trailing fields (backward-compatible)
        let (array_ndims, array_element_type) = if bytes.len() > consumed + 1 {
            let ndims = bytes[consumed];
            if ndims > 0 {
                if bytes.len() < consumed + 2 {
                    return Err(err());
                }
                let elem = ColumnType::try_from(bytes[consumed + 1])?;
                consumed += 2;
                (Some(ndims), Some(elem))
            } else {
                consumed += 1;
                (None, None)
            }
        } else {
            // Legacy row: no array trailing fields
            (None, None)
        };

        // Phase 24.1c: identity kind trailing byte (backward-compatible).
        // Pre-24.1c rows stop here; new rows write one byte only when the
        // column has a non-`None` identity kind. Unknown bytes fall back
        // to `IdentityKind::None` (via the `TryFrom` impl), keeping us
        // forward-compatible with future trailing fields.
        let identity_kind = if bytes.len() > consumed {
            let kind = IdentityKind::try_from(bytes[consumed]).unwrap_or(IdentityKind::None);
            if kind != IdentityKind::None {
                consumed += 1;
            }
            kind
        } else {
            IdentityKind::None
        };

        Ok((
            Self {
                table_id,
                col_idx,
                name,
                col_type,
                nullable,
                auto_increment,
                type_len,
                is_fixed_len,
                default_expr,
                on_update_expr,
                generated_expr,
                collation,
                generated_stored,
                enum_type_name,
                array_element_type,
                array_ndims,
                identity_kind,
            },
            consumed,
        ))
    }
}
