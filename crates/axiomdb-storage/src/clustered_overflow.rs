//! Overflow-page chains for clustered rows whose logical payload no longer
//! fits fully inline inside a clustered leaf cell.

use std::collections::HashSet;

use axiomdb_core::error::DbError;

use crate::{
    page::{Page, PageType, HEADER_SIZE, PAGE_SIZE},
    StorageEngine,
};

/// Sentinel page id meaning "end of overflow chain".
pub const NULL_PAGE: u64 = u64::MAX;

const NEXT_PAGE_SIZE: usize = std::mem::size_of::<u64>();

/// Bytes available for payload on one overflow page.
pub fn payload_capacity() -> usize {
    PAGE_SIZE - HEADER_SIZE - NEXT_PAGE_SIZE
}

/// Writes `payload` into a newly allocated overflow-page chain.
///
/// Returns `Ok(None)` when `payload` is empty.
pub fn write_chain(
    storage: &mut dyn StorageEngine,
    payload: &[u8],
) -> Result<Option<u64>, DbError> {
    if payload.is_empty() {
        return Ok(None);
    }

    let chunks: Vec<&[u8]> = payload.chunks(payload_capacity()).collect();
    let mut page_ids = Vec::with_capacity(chunks.len());

    for _ in &chunks {
        match storage.alloc_page(PageType::Overflow) {
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
pub fn free_chain(storage: &mut dyn StorageEngine, first_page_id: u64) -> Result<(), DbError> {
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

fn cleanup_allocated_pages(
    storage: &mut dyn StorageEngine,
    page_ids: &[u64],
) -> Result<(), DbError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryStorage, PageType, StorageEngine};

    #[test]
    fn write_and_read_single_page_chain_roundtrips() {
        let mut storage = MemoryStorage::new();
        let payload = vec![0xAB; payload_capacity() / 2];
        let first = write_chain(&mut storage, &payload).unwrap().unwrap();

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
        let mut storage = MemoryStorage::new();
        let payload = vec![0x5C; payload_capacity() * 3 + 127];
        let first = write_chain(&mut storage, &payload).unwrap().unwrap();

        let roundtrip = read_chain(&storage, first, payload.len()).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn free_chain_releases_every_page() {
        let mut storage = MemoryStorage::new();
        let payload = vec![0x9D; payload_capacity() * 2 + 11];
        let first = write_chain(&mut storage, &payload).unwrap().unwrap();

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

        free_chain(&mut storage, first).unwrap();
        for page_id in pages {
            assert!(storage.read_page(page_id).is_err());
        }
    }
}
