# Spec: WAL Entry Format (Subfase 3.1)

## What to build (not how)

The `WalEntry` type and its binary serialization/deserialization. Defines the exact on-disk
layout of each Write-Ahead Log record: fields, sizes, byte order, and integrity checksum.
Does not include I/O (that is 3.2 WalWriter and 3.3 WalReader).

---

## Fixed design decisions

| Aspect | Decision | Reason |
|---|---|---|
| Granularity | **Logical** (semantic operation) | Small entries, recovery replays the operation, compatible with replication (Phase 18) and MVCC (Phase 7) |
| WAL scope | **Global** (`nexusdb.wal`) | A single atomic LSN, a single fsync, multi-table transactions without coordination |
| Value encoding | **Raw bytes** (`&[u8]`) | No type overhead, extensible: today RecordId (10B), in Phase 4 full rows |
| Endianness | **Little-endian** | Consistent with the rest of the codebase (PageHeader, page_layout) |
| Checksum | **CRC32c** | Same algorithm as pages — a single dependency |
| Backward scan | **entry_len repeated at the end** | Allows traversing the WAL backwards during ROLLBACK and crash recovery |

---

## Binary layout of WalEntry

```
Offset    Size    Field          Type    Description
       0       4  entry_len      u32 LE  Total length of the entry (all bytes including this field and the trailing one)
       4       8  lsn            u64 LE  Log Sequence Number — globally monotonic, never repeated
      12       8  txn_id         u64 LE  Transaction ID (0 = autocommit / no explicit transaction)
      20       1  entry_type     u8      Operation type (see EntryType below)
      21       4  table_id       u32 LE  Table identifier (0 = system/meta)
      25       2  key_len        u16 LE  Length of the key in bytes (0 in BEGIN/COMMIT/ROLLBACK/CHECKPOINT)
      27  key_len  key           [u8]    Key bytes — up to MAX_KEY_LEN (64 bytes in Phase 3)
       ?       4  old_val_len    u32 LE  Length of the previous value (0 in INSERT, CHECKPOINT, txn control)
       ?  old_len  old_value     [u8]    Previous value serialized (RecordId in Phase 3, Row in Phase 4+)
       ?       4  new_val_len    u32 LE  Length of the new value (0 in DELETE, txn control)
       ?  new_len  new_value     [u8]    New value serialized
       ?       4  crc32c         u32 LE  CRC32c of all preceding bytes in this entry
       ?       4  entry_len_2    u32 LE  Repetition of entry_len — allows backward scan without reading everything
```

**Minimum size** (BEGIN / COMMIT / ROLLBACK with key_len=0, old=0, new=0):
`4 + 8 + 8 + 1 + 4 + 2 + 0 + 4 + 0 + 4 + 0 + 4 + 4 = 43 bytes`

**Typical size** (INSERT with 8-byte key, RecordId 10 bytes):
`43 + 8 + 10 = 61 bytes`

**Why table_id is u32 and not u16:**
u16 limits to 65535 tables. Real databases (multi-schema, partitioned, internal catalog)
can exceed that limit. u32 costs 2 extra bytes but eliminates a structural limitation.

**Why entry_len_2 at the end:**
The WalReader in crash recovery needs to traverse the WAL backwards to discard entries
without COMMIT. With entry_len at the start you move forward; with entry_len_2 at the end you move back:
`prev_pos = curr_pos - entry_len_2`.

---

## EntryType — entry types

```rust
#[repr(u8)]
pub enum EntryType {
    Begin      = 1,  // start of an explicit transaction
    Commit     = 2,  // transaction commit
    Rollback   = 3,  // transaction rollback
    Insert     = 4,  // insert key+value (old_val_len=0)
    Delete     = 5,  // delete key (new_val_len=0, old_value = value before the delete)
    Update     = 6,  // update key: old_value → new_value
    Checkpoint = 7,  // checkpoint marker (key=0, old=0, new=0)
}
```

**Field semantics by type:**

| EntryType | key | old_value | new_value |
|---|---|---|---|
| Begin | empty | empty | empty |
| Commit | empty | empty | empty |
| Rollback | empty | empty | empty |
| Insert | inserted key | empty | new value |
| Delete | deleted key | value before the delete | empty |
| Update | updated key | previous value | new value |
| Checkpoint | empty | empty | empty |

---

## Rust struct

```rust
/// Write-Ahead Log entry.
///
/// Represents a logical operation. Serialized to bytes with `WalEntry::to_bytes()`
/// and deserialized with `WalEntry::from_bytes()`.
pub struct WalEntry {
    pub lsn:        u64,
    pub txn_id:     u64,
    pub entry_type: EntryType,
    pub table_id:   u32,
    pub key:        Vec<u8>,
    pub old_value:  Vec<u8>,
    pub new_value:  Vec<u8>,
}
```

The `entry_len` field does not appear in the struct — it is computed in `to_bytes()` and verified in
`from_bytes()`. CRC32c is also not stored in memory — it is computed during serialization and
verified during deserialization.

---

## Inputs / Outputs

### `WalEntry::to_bytes() -> Vec<u8>`
- Input: `&WalEntry`
- Output: bytes ready to write to the WAL file (includes entry_len, CRC, entry_len_2)
- Errors: none — serialization is infallible if the entry is valid

### `WalEntry::from_bytes(buf: &[u8]) -> Result<(WalEntry, usize), DbError>`
- Input: byte slice (may contain more than one entry)
- Output: `(entry, bytes_consumed)` — allows parsing chained entries
- Errors:
  - `WalChecksumMismatch { lsn, expected, got }` — CRC does not match
  - `WalEntryTruncated { lsn }` — buffer ends before the entry is complete
  - `WalUnknownEntryType { byte }` — unknown type byte

### `WalEntry::serialized_len(&self) -> usize`
- Computes the total serialized size without allocating — used by WalWriter to pre-allocate the buffer

---

## Use cases

1. **Serialize INSERT**: key=`b"user:001"`, new_value=`RecordId{page:5, slot:3}` serialized to 10B → correct bytes, valid CRC
2. **Deserialize roundtrip**: `to_bytes()` → `from_bytes()` produces the identical original entry
3. **Detect corruption**: modify 1 byte in the payload → `from_bytes()` returns `WalChecksumMismatch`
4. **Truncated buffer**: pass fewer bytes than the entry requires → `WalEntryTruncated`
5. **Unknown type**: type byte `0xFF` → `WalUnknownEntryType`
6. **Minimum entry**: BEGIN with key=[], old=[], new=[] → exactly 43 bytes
7. **Backward scan**: given `entry_len_2` at the end, compute the offset of the previous entry

---

## Acceptance criteria

- [ ] `WalEntry` struct public with the 7 fields
- [ ] `EntryType` enum `#[repr(u8)]` with the 7 types
- [ ] `WalEntry::to_bytes()` produces the exact layout described (verify offsets with test)
- [ ] `WalEntry::from_bytes()` parses correctly and returns `bytes_consumed`
- [ ] `to_bytes()` → `from_bytes()` roundtrip produces an identical entry for all 7 types
- [ ] CRC32c covers all bytes before the crc32c field (not the crc itself nor entry_len_2)
- [ ] 1-byte corruption in payload detected by CRC → `WalChecksumMismatch`
- [ ] Truncated buffer → `WalEntryTruncated`
- [ ] `entry_len == entry_len_2` in every serialized entry (invariant)
- [ ] `serialized_len()` == `to_bytes().len()` for all types
- [ ] Error variants are in `DbError` (no new types)
- [ ] Zero `unwrap()` in production code
- [ ] Zero `unsafe`

---

## Out of scope

- Disk I/O (WalWriter — subfase 3.2)
- Reading from file (WalReader — subfase 3.3)
- BEGIN / COMMIT / ROLLBACK SQL (subfase 3.4)
- Crash recovery (subfase 3.5)
- Per-table WAL (discarded decision — WAL is global)
- Entry compression (future phase if the WAL grows too large)
- Entries larger than 4GB (u32 for lengths is sufficient for any reasonable SQL operation)

---

## Dependencies

- `nexusdb-core`: `DbError` (add variants `WalChecksumMismatch`, `WalEntryTruncated`, `WalUnknownEntryType`)
- `crc32c` (already in workspace since Phase 1)
- Does not depend on `nexusdb-storage` or `nexusdb-index`
