//! Overflow-page chains for clustered rows whose logical payload no longer
//! fits fully inline inside a clustered leaf cell.

use std::collections::HashSet;

use axiomdb_core::error::DbError;

use crate::{
    local_page_batch::{batch_alloc_page, LocalPageBatch},
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
    StorageEngine,
};

/// Sentinel page id meaning "end of overflow chain".
pub const NULL_PAGE: u64 = u64::MAX;

const NEXT_PAGE_SIZE: usize = std::mem::size_of::<u64>();
const BLOB_MAGIC: [u8; 4] = *b"ABOB";
const BLOB_VERSION: u8 = 1;
const BLOB_FLAGS_FIRST: u8 = 0x01;
const BLOB_MAGIC_OFF: usize = 0;
const BLOB_VERSION_OFF: usize = 4;
const BLOB_FLAGS_OFF: usize = 5;
const BLOB_RESERVED_OFF: usize = 6;
const BLOB_NEXT_OFF: usize = 8;
const BLOB_PART_LEN_OFF: usize = 16;
const BLOB_REFCOUNT_OFF: usize = 20;
const BLOB_HEADER_SIZE: usize = 28;

/// Bytes available for payload on one overflow page.
pub fn payload_capacity() -> usize {
    PAGE_SIZE - HEADER_SIZE - NEXT_PAGE_SIZE
}

/// Bytes available for payload on one refcounted TOAST/BLOB overflow page.
pub fn blob_payload_capacity() -> usize {
    PAGE_SIZE - HEADER_SIZE - BLOB_HEADER_SIZE
}

/// Writes `payload` into a newly allocated overflow-page chain.
///
/// Returns `Ok(None)` when `payload` is empty.
pub fn write_chain(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    payload: &[u8],
) -> Result<Option<u64>, DbError> {
    if payload.is_empty() {
        return Ok(None);
    }

    let chunks: Vec<&[u8]> = payload.chunks(payload_capacity()).collect();
    let mut page_ids = Vec::with_capacity(chunks.len());
    let mut batch = batch;

    for _ in &chunks {
        match batch_alloc_page(storage, batch.as_deref_mut(), PageType::Overflow) {
            Ok(page_id) => page_ids.push(page_id),
            Err(err) => {
                cleanup_allocated_pages(storage, &page_ids)?;
                return Err(err);
            }
        }
    }

    for (idx, chunk) in chunks.iter().enumerate() {
        let page_id = page_ids[idx];
        let next = page_ids.get(idx + 1).copied().unwrap_or(NULL_PAGE);
        let mut page = Page::new(PageType::Overflow, page_id);
        set_next_page(&mut page, next);
        let body = page.body_mut();
        let payload_start = NEXT_PAGE_SIZE;
        body[payload_start..payload_start + chunk.len()].copy_from_slice(chunk);
        page.update_checksum();
        if let Err(err) = storage.write_page(page_id, &page) {
            cleanup_allocated_pages(storage, &page_ids)?;
            return Err(err);
        }
    }

    Ok(page_ids.first().copied())
}

/// Reads exactly `expected_len` bytes from an overflow-page chain.
pub fn read_chain(
    storage: &dyn StorageEngine,
    first_page_id: u64,
    expected_len: usize,
) -> Result<Vec<u8>, DbError> {
    if expected_len == 0 {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered overflow chain starts at page {first_page_id} but expected tail length is zero"
            ),
        });
    }

    let mut page_id = first_page_id;
    let mut remaining = expected_len;
    let mut out = Vec::with_capacity(expected_len);
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(page_id) {
            return Err(DbError::BTreeCorrupted {
                msg: format!("clustered overflow chain loop detected at page {page_id}"),
            });
        }

        let page = storage.read_page(page_id)?;
        ensure_overflow_page(&page, page_id)?;

        let next = next_page(&page);
        let chunk_len = remaining.min(payload_capacity());
        let body = page.body();
        let payload_start = NEXT_PAGE_SIZE;
        let payload_end = payload_start + chunk_len;
        out.extend_from_slice(&body[payload_start..payload_end]);
        remaining -= chunk_len;

        if remaining == 0 {
            if next != NULL_PAGE {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered overflow chain at page {page_id} has trailing page {next} beyond expected length {expected_len}"
                    ),
                });
            }
            return Ok(out);
        }

        if next == NULL_PAGE {
            return Err(DbError::BTreeCorrupted {
                msg: format!(
                    "clustered overflow chain ended early at page {page_id}: {remaining} bytes still missing"
                ),
            });
        }
        page_id = next;
    }
}

/// Frees every page in an overflow-page chain.
pub fn free_chain(storage: &dyn StorageEngine, first_page_id: u64) -> Result<(), DbError> {
    let mut page_id = first_page_id;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(page_id) {
            return Err(DbError::BTreeCorrupted {
                msg: format!("clustered overflow free detected loop at page {page_id}"),
            });
        }

        let page = storage.read_page(page_id)?;
        ensure_overflow_page(&page, page_id)?;
        let next = next_page(&page);
        storage.free_page(page_id)?;
        if next == NULL_PAGE {
            return Ok(());
        }
        page_id = next;
    }
}

/// Writes `payload` into a refcounted TOAST/BLOB overflow chain.
///
/// Returns `Ok(None)` when `payload` is empty. The first page starts with
/// `refcount = 1`; continuation pages carry the same versioned header but a
/// zero refcount field.
pub fn write_refcounted_chain(
    storage: &dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    payload: &[u8],
) -> Result<Option<u64>, DbError> {
    if payload.is_empty() {
        return Ok(None);
    }

    let chunks: Vec<&[u8]> = payload.chunks(blob_payload_capacity()).collect();
    let mut page_ids = Vec::with_capacity(chunks.len());
    let mut batch = batch;

    for _ in &chunks {
        match batch_alloc_page(storage, batch.as_deref_mut(), PageType::Overflow) {
            Ok(page_id) => page_ids.push(page_id),
            Err(err) => {
                cleanup_allocated_pages(storage, &page_ids)?;
                return Err(err);
            }
        }
    }

    for (idx, chunk) in chunks.iter().enumerate() {
        let page_id = page_ids[idx];
        let next = page_ids.get(idx + 1).copied().unwrap_or(NULL_PAGE);
        let mut page = Page::new(PageType::Overflow, page_id);
        set_blob_header(
            &mut page,
            next,
            chunk.len(),
            if idx == 0 { 1 } else { 0 },
            idx == 0,
        )?;
        let body = page.body_mut();
        let payload_start = BLOB_HEADER_SIZE;
        body[payload_start..payload_start + chunk.len()].copy_from_slice(chunk);
        page.update_checksum();
        if let Err(err) = storage.write_page(page_id, &page) {
            cleanup_allocated_pages(storage, &page_ids)?;
            return Err(err);
        }
    }

    Ok(page_ids.first().copied())
}

/// Reads a TOAST/BLOB overflow chain.
///
/// Refcounted chains are self-delimiting through each page's `part_len`.
/// Legacy chains fall back to [`read_chain`] and therefore need the expected
/// payload length from the old inline pointer.
pub fn read_blob_chain(
    storage: &dyn StorageEngine,
    first_page_id: u64,
    legacy_expected_len: usize,
) -> Result<Vec<u8>, DbError> {
    let first = storage.read_page(first_page_id)?;
    ensure_overflow_page(&first, first_page_id)?;
    if !is_refcounted_blob_page(&first) {
        return read_chain(storage, first_page_id, legacy_expected_len);
    }

    let mut page_id = first_page_id;
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    let mut next_page_ref = Some(first);
    let mut is_first = true;

    loop {
        if !visited.insert(page_id) {
            return Err(DbError::BTreeCorrupted {
                msg: format!("refcounted blob chain loop detected at page {page_id}"),
            });
        }

        let page = if let Some(page) = next_page_ref.take() {
            page
        } else {
            storage.read_page(page_id)?
        };
        ensure_blob_page(&page, page_id, is_first)?;

        let next = blob_next_page(&page);
        let part_len = blob_part_len(&page) as usize;
        if part_len > blob_payload_capacity() {
            return Err(DbError::BTreeCorrupted {
                msg: format!("refcounted blob page {page_id} has invalid part length {part_len}"),
            });
        }
        let body = page.body();
        out.extend_from_slice(&body[BLOB_HEADER_SIZE..BLOB_HEADER_SIZE + part_len]);

        if next == NULL_PAGE {
            return Ok(out);
        }
        page_id = next;
        is_first = false;
    }
}

/// Increments a refcounted TOAST/BLOB chain's first-page refcount.
pub fn incref_blob(storage: &dyn StorageEngine, first_page_id: u64) -> Result<u64, DbError> {
    let mut page = storage.read_page(first_page_id)?.into_page();
    ensure_blob_page(&page, first_page_id, true)?;
    let refcount = blob_refcount(&page);
    if refcount == 0 {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob page {first_page_id} has zero refcount"),
        });
    }
    let new_refcount = refcount
        .checked_add(1)
        .ok_or_else(|| DbError::BTreeCorrupted {
            msg: format!("refcounted blob page {first_page_id} refcount overflow"),
        })?;
    set_blob_refcount(&mut page, new_refcount);
    page.update_checksum();
    storage.write_page(first_page_id, &page)?;
    Ok(new_refcount)
}

/// Releases one TOAST/BLOB reference. Frees the chain only when refcount reaches zero.
pub fn free_blob(storage: &dyn StorageEngine, first_page_id: u64) -> Result<(), DbError> {
    let mut first = storage.read_page(first_page_id)?.into_page();
    ensure_overflow_page(&first, first_page_id)?;
    if !is_refcounted_blob_page(&first) {
        return free_chain(storage, first_page_id);
    }

    ensure_blob_page(&first, first_page_id, true)?;
    let refcount = blob_refcount(&first);
    if refcount == 0 {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob page {first_page_id} has zero refcount"),
        });
    }
    if refcount > 1 {
        let new_refcount = refcount - 1;
        set_blob_refcount(&mut first, new_refcount);
        first.update_checksum();
        storage.write_page(first_page_id, &first)?;
        return Ok(());
    }

    let mut page_id = first_page_id;
    let mut next_page = Some(first);
    let mut visited = HashSet::new();
    let mut is_first = true;

    loop {
        if !visited.insert(page_id) {
            return Err(DbError::BTreeCorrupted {
                msg: format!("refcounted blob free detected loop at page {page_id}"),
            });
        }

        let page = if let Some(page) = next_page.take() {
            page
        } else {
            storage.read_page(page_id)?.into_page()
        };
        ensure_blob_page(&page, page_id, is_first)?;
        let next = blob_next_page(&page);
        storage.free_page(page_id)?;
        if next == NULL_PAGE {
            return Ok(());
        }
        page_id = next;
        is_first = false;
    }
}

fn cleanup_allocated_pages(storage: &dyn StorageEngine, page_ids: &[u64]) -> Result<(), DbError> {
    for &page_id in page_ids {
        storage.free_page(page_id)?;
    }
    Ok(())
}

fn ensure_overflow_page(page: &Page, page_id: u64) -> Result<(), DbError> {
    let page_type =
        PageType::try_from(page.header().page_type).map_err(|err| DbError::BTreeCorrupted {
            msg: format!("clustered overflow page {page_id} has invalid page type byte: {err}"),
        })?;
    if page_type != PageType::Overflow {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered overflow chain expected overflow page at {page_id}, found {page_type:?}"
            ),
        });
    }
    Ok(())
}

fn next_page(page: &Page) -> u64 {
    let body = page.body();
    // body() is always PAGE_BODY_SIZE (16,320 bytes) so the first 8 bytes are
    // guaranteed to exist.  Use a fixed-size array reference instead of
    // try_into() to avoid the need for expect/unwrap.
    let bytes: [u8; 8] = [
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ];
    u64::from_le_bytes(bytes)
}

fn set_next_page(page: &mut Page, next: u64) {
    page.body_mut()[..NEXT_PAGE_SIZE].copy_from_slice(&next.to_le_bytes());
}

fn is_refcounted_blob_page(page: &Page) -> bool {
    let body = page.body();
    body[BLOB_MAGIC_OFF..BLOB_MAGIC_OFF + BLOB_MAGIC.len()] == BLOB_MAGIC
}

fn ensure_blob_page(page: &Page, page_id: u64, must_be_first: bool) -> Result<(), DbError> {
    ensure_overflow_page(page, page_id)?;
    let body = page.body();
    if body[BLOB_MAGIC_OFF..BLOB_MAGIC_OFF + BLOB_MAGIC.len()] != BLOB_MAGIC {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob chain expected blob page at {page_id}"),
        });
    }
    if body[BLOB_VERSION_OFF] != BLOB_VERSION {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "refcounted blob page {page_id} has unsupported version {}",
                body[BLOB_VERSION_OFF]
            ),
        });
    }
    let flags = body[BLOB_FLAGS_OFF];
    if flags & !BLOB_FLAGS_FIRST != 0 {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob page {page_id} has invalid flags {flags:#04x}"),
        });
    }
    let is_first = flags & BLOB_FLAGS_FIRST != 0;
    if must_be_first && !is_first {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob chain first page {page_id} lacks first-page flag"),
        });
    }
    if !must_be_first && is_first {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob continuation page {page_id} has first-page flag"),
        });
    }
    if !must_be_first && blob_refcount(page) != 0 {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob continuation page {page_id} has nonzero refcount"),
        });
    }
    Ok(())
}

fn blob_next_page(page: &Page) -> u64 {
    let body = page.body();
    u64::from_le_bytes([
        body[BLOB_NEXT_OFF],
        body[BLOB_NEXT_OFF + 1],
        body[BLOB_NEXT_OFF + 2],
        body[BLOB_NEXT_OFF + 3],
        body[BLOB_NEXT_OFF + 4],
        body[BLOB_NEXT_OFF + 5],
        body[BLOB_NEXT_OFF + 6],
        body[BLOB_NEXT_OFF + 7],
    ])
}

fn blob_part_len(page: &Page) -> u32 {
    let body = page.body();
    u32::from_le_bytes([
        body[BLOB_PART_LEN_OFF],
        body[BLOB_PART_LEN_OFF + 1],
        body[BLOB_PART_LEN_OFF + 2],
        body[BLOB_PART_LEN_OFF + 3],
    ])
}

fn blob_refcount(page: &Page) -> u64 {
    let body = page.body();
    u64::from_le_bytes([
        body[BLOB_REFCOUNT_OFF],
        body[BLOB_REFCOUNT_OFF + 1],
        body[BLOB_REFCOUNT_OFF + 2],
        body[BLOB_REFCOUNT_OFF + 3],
        body[BLOB_REFCOUNT_OFF + 4],
        body[BLOB_REFCOUNT_OFF + 5],
        body[BLOB_REFCOUNT_OFF + 6],
        body[BLOB_REFCOUNT_OFF + 7],
    ])
}

fn set_blob_refcount(page: &mut Page, refcount: u64) {
    page.body_mut()[BLOB_REFCOUNT_OFF..BLOB_REFCOUNT_OFF + 8]
        .copy_from_slice(&refcount.to_le_bytes());
}

fn set_blob_header(
    page: &mut Page,
    next: u64,
    part_len: usize,
    refcount: u64,
    is_first: bool,
) -> Result<(), DbError> {
    if part_len > blob_payload_capacity() {
        return Err(DbError::BTreeCorrupted {
            msg: format!("refcounted blob part length {part_len} exceeds page capacity"),
        });
    }
    let body = page.body_mut();
    body[BLOB_MAGIC_OFF..BLOB_MAGIC_OFF + BLOB_MAGIC.len()].copy_from_slice(&BLOB_MAGIC);
    body[BLOB_VERSION_OFF] = BLOB_VERSION;
    body[BLOB_FLAGS_OFF] = if is_first { BLOB_FLAGS_FIRST } else { 0 };
    body[BLOB_RESERVED_OFF..BLOB_RESERVED_OFF + 2].copy_from_slice(&0u16.to_le_bytes());
    body[BLOB_NEXT_OFF..BLOB_NEXT_OFF + 8].copy_from_slice(&next.to_le_bytes());
    body[BLOB_PART_LEN_OFF..BLOB_PART_LEN_OFF + 4]
        .copy_from_slice(&(part_len as u32).to_le_bytes());
    body[BLOB_REFCOUNT_OFF..BLOB_REFCOUNT_OFF + 8].copy_from_slice(&refcount.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryStorage, PageType, StorageEngine};

    #[test]
    fn write_and_read_single_page_chain_roundtrips() {
        let storage = MemoryStorage::new();
        let payload = vec![0xAB; payload_capacity() / 2];
        let first = write_chain(&storage, None, &payload).unwrap().unwrap();

        let page = storage.read_page(first).unwrap();
        assert_eq!(
            PageType::try_from(page.header().page_type).unwrap(),
            PageType::Overflow
        );

        let roundtrip = read_chain(&storage, first, payload.len()).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn write_and_read_multi_page_chain_roundtrips() {
        let storage = MemoryStorage::new();
        let payload = vec![0x5C; payload_capacity() * 3 + 127];
        let first = write_chain(&storage, None, &payload).unwrap().unwrap();

        let roundtrip = read_chain(&storage, first, payload.len()).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn free_chain_releases_every_page() {
        let storage = MemoryStorage::new();
        let payload = vec![0x9D; payload_capacity() * 2 + 11];
        let first = write_chain(&storage, None, &payload).unwrap().unwrap();

        let mut pages = Vec::new();
        let mut current = first;
        loop {
            pages.push(current);
            let page = storage.read_page(current).unwrap();
            let next = next_page(&page);
            if next == NULL_PAGE {
                break;
            }
            current = next;
        }

        free_chain(&storage, first).unwrap();
        for page_id in pages {
            assert!(storage.read_page(page_id).is_err());
        }
    }

    #[test]
    fn refcounted_blob_single_page_roundtrips() {
        let storage = MemoryStorage::new();
        let payload = vec![0xD1; blob_payload_capacity() / 2];
        let first = write_refcounted_chain(&storage, None, &payload)
            .unwrap()
            .unwrap();

        let page = storage.read_page(first).unwrap();
        assert!(is_refcounted_blob_page(&page));
        assert_eq!(blob_refcount(&page), 1);

        let roundtrip = read_blob_chain(&storage, first, payload.len()).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn refcounted_blob_multi_page_roundtrips() {
        let storage = MemoryStorage::new();
        let payload = vec![0x6E; blob_payload_capacity() * 2 + 333];
        let first = write_refcounted_chain(&storage, None, &payload)
            .unwrap()
            .unwrap();

        let roundtrip = read_blob_chain(&storage, first, payload.len()).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn free_blob_decrements_before_freeing_chain() {
        let storage = MemoryStorage::new();
        let payload = vec![0xA7; blob_payload_capacity() + 19];
        let first = write_refcounted_chain(&storage, None, &payload)
            .unwrap()
            .unwrap();
        assert_eq!(incref_blob(&storage, first).unwrap(), 2);

        free_blob(&storage, first).unwrap();
        assert_eq!(blob_refcount(&storage.read_page(first).unwrap()), 1);
        assert_eq!(
            read_blob_chain(&storage, first, payload.len()).unwrap(),
            payload
        );

        free_blob(&storage, first).unwrap();
        assert!(storage.read_page(first).is_err());
    }

    #[test]
    fn free_blob_handles_legacy_chain() {
        let storage = MemoryStorage::new();
        let payload = vec![0x3B; payload_capacity() + 77];
        let first = write_chain(&storage, None, &payload).unwrap().unwrap();

        assert_eq!(
            read_blob_chain(&storage, first, payload.len()).unwrap(),
            payload
        );
        free_blob(&storage, first).unwrap();
        assert!(storage.read_page(first).is_err());
    }

    #[test]
    fn free_blob_rejects_zero_refcount() {
        let storage = MemoryStorage::new();
        let payload = vec![0xF0; blob_payload_capacity() / 3];
        let first = write_refcounted_chain(&storage, None, &payload)
            .unwrap()
            .unwrap();
        let mut page = storage.read_page(first).unwrap().into_page();
        set_blob_refcount(&mut page, 0);
        page.update_checksum();
        storage.write_page(first, &page).unwrap();

        let err = free_blob(&storage, first).unwrap_err();
        assert!(matches!(err, DbError::BTreeCorrupted { .. }));
    }

    #[test]
    fn read_blob_rejects_nonzero_continuation_refcount() {
        let storage = MemoryStorage::new();
        let payload = vec![0xB5; blob_payload_capacity() + 1];
        let first = write_refcounted_chain(&storage, None, &payload)
            .unwrap()
            .unwrap();
        let next = blob_next_page(&storage.read_page(first).unwrap());
        let mut page = storage.read_page(next).unwrap().into_page();
        set_blob_refcount(&mut page, 1);
        page.update_checksum();
        storage.write_page(next, &page).unwrap();

        let err = read_blob_chain(&storage, first, payload.len()).unwrap_err();
        assert!(matches!(err, DbError::BTreeCorrupted { .. }));
    }
}
