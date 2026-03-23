# Plan: WAL Entry Format (Subfase 3.1)

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/axiomdb-core/src/error.rs` | Modify | Add 3 WAL variants to DbError |
| `crates/axiomdb-wal/Cargo.toml` | Modify | Add `crc32c` dependency |
| `crates/axiomdb-wal/src/lib.rs` | Replace | Public modules of the crate |
| `crates/axiomdb-wal/src/entry.rs` | Create | `EntryType`, `WalEntry`, serialization |
| `crates/axiomdb-wal/tests/integration_wal_entry.rs` | Create | Integration tests |

---

## Serialization algorithm — `WalEntry::to_bytes()`

```
1. Compute entry_len = total size (see formula below)
2. Reserve Vec<u8> with capacity entry_len
3. Write entry_len         (4 bytes LE)
4. Write lsn               (8 bytes LE)
5. Write txn_id            (8 bytes LE)
6. Write entry_type as u8  (1 byte)
7. Write table_id          (4 bytes LE)
8. Write key_len as u16    (2 bytes LE)
9. Write key bytes         (key_len bytes)
10. Write old_val_len      (4 bytes LE)
11. Write old_value bytes  (old_val_len bytes)
12. Write new_val_len      (4 bytes LE)
13. Write new_value bytes  (new_val_len bytes)
14. Compute CRC32c of buf[0..pos]
15. Write crc32c           (4 bytes LE)
16. Write entry_len        (4 bytes LE) — copy for backward scan
```

**entry_len formula:**
```
4 + 8 + 8 + 1 + 4 + 2 + key.len() + 4 + old.len() + 4 + new.len() + 4 + 4
= 43 + key.len() + old.len() + new.len()
```

**Constant MIN_ENTRY_LEN = 43** (entry with no key or values).

---

## Deserialization algorithm — `WalEntry::from_bytes(buf)`

```
1. Verify buf.len() >= 4 (at least entry_len) → WalEntryTruncated if not
2. Read entry_len (4 bytes LE)
3. Verify buf.len() >= entry_len → WalEntryTruncated if not
4. Read lsn, txn_id, entry_type, table_id, key_len
5. Verify entry_type is a known value → WalUnknownEntryType if not
6. Read key[0..key_len]
7. Read old_val_len, old_value[0..old_val_len]
8. Read new_val_len, new_value[0..new_val_len]
9. Read expected crc32c
10. Compute CRC32c of buf[0..pos_before_crc]
11. Compare → WalChecksumMismatch if they don't match
12. Read entry_len_2 (ignore — only used for external backward scan)
13. Verify entry_len_2 == entry_len → WalEntryTruncated if they don't match
14. Return (WalEntry, entry_len)
```

---

## Implementation phases

### Step 1 — Add WAL errors to DbError
In `axiomdb-core/src/error.rs`, section `// ── WAL`:
```rust
#[error("WAL entry at LSN {lsn} has invalid checksum: expected {expected:#010x}, got {got:#010x}")]
WalChecksumMismatch { lsn: u64, expected: u32, got: u32 },

#[error("WAL entry at LSN {lsn} is truncated — the file may be corrupt")]
WalEntryTruncated { lsn: u64 },

#[error("WAL entry has unknown type: {byte:#04x}")]
WalUnknownEntryType { byte: u8 },
```

### Step 2 — Add crc32c to axiomdb-wal/Cargo.toml
```toml
crc32c = "0.6"
```
(same version as axiomdb-storage — already tested)

### Step 3 — Implement `entry.rs`
In order:
1. `EntryType` enum `#[repr(u8)]` with `TryFrom<u8>`
2. `WalEntry` struct with the 7 public fields
3. `impl WalEntry` with:
   - `pub fn new(...)` constructor
   - `pub fn serialized_len(&self) -> usize`
   - `pub fn to_bytes(&self) -> Vec<u8>`
   - `pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize), DbError>`
4. Inline unit tests `#[cfg(test)]`

### Step 4 — Update `lib.rs`
Expose the module and public types.

### Step 5 — Integration tests
In `tests/integration_wal_entry.rs`:
- Roundtrip for all 7 entry types
- Corruption detection (bit flip in key, payload, header)
- Truncated buffer (buf shorter than entry_len)
- Multiple chained entries (parse N entries from a buffer)
- Backward scan: verify entry_len == entry_len_2

---

## Tests to write

**Unit tests (in entry.rs `#[cfg(test)]`):**
- `test_entry_type_roundtrip` — `u8 → EntryType → u8` for all 7 types
- `test_entry_type_unknown` — invalid byte → `WalUnknownEntryType`
- `test_serialized_len_matches_to_bytes` — `serialized_len() == to_bytes().len()`
- `test_min_entry_len_is_43` — serialized BEGIN has exactly 43 bytes
- `test_entry_len_repeated_at_end` — last 4 bytes == first 4 bytes

**Integration tests (in tests/):**
- `test_roundtrip_all_entry_types` — to_bytes → from_bytes for Begin, Commit, Rollback, Insert, Delete, Update, Checkpoint
- `test_crc_corruption_detected` — 1-bit flip in key, old_value, new_value → WalChecksumMismatch
- `test_header_corruption_detected` — flip in txn_id or table_id → WalChecksumMismatch
- `test_truncated_buffer` — buf[..entry_len-1] → WalEntryTruncated
- `test_empty_buffer` — buf[..0] → WalEntryTruncated (lsn=0)
- `test_chain_of_entries` — serialize 100 entries, parse all in a loop
- `test_backward_scan_offset` — entry_len_2 allows correctly computing the previous offset

---

## Anti-patterns to avoid

- **DO NOT use `unwrap()`** in production code — everything uses `?` or `map_err`
- **DO NOT use `unsafe`** — no pointer casts, only slices and LE bytes
- **DO NOT serialize with serde** — the binary format is manual for full layout control
- **DO NOT store crc or entry_len in the struct** — computed during serialize/deserialize
- **DO NOT assume alignment** — read with `from_le_bytes([buf[i], buf[i+1], ...])`, same as decode_rid

---

## Risks

| Risk | Mitigation |
|---|---|
| CRC accidentally covers entry_len_2 | CRC is computed before writing entry_len_2 — test verifies this |
| key_len exceeds MAX_KEY_LEN in Phase 3 | `from_bytes` does not validate key_len against MAX_KEY_LEN — that is the WalWriter's responsibility. The format is size-agnostic |
| entry_len overflow with very large values | u32 supports up to 4GB per entry — sufficient for any SQL operation |
| LSN=0 is ambiguous (entry without assigned LSN) | The WalWriter assigns LSN — `WalEntry::new` receives lsn as a parameter. LSN=0 only in tests |
