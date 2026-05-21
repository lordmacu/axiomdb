//! Reference-counted page handle returned by [`StorageEngine::read_page`].
//!
//! `PageRef` wraps an `Arc<Page>` and derefs to `&Page`, so callers use it
//! transparently wherever `&Page` was expected. Backing the page with an `Arc`
//! makes [`PageRef::clone`] O(1) (an atomic refcount bump, no 16 KB copy),
//! which is what lets the buffer pool serve a cache hit without re-materializing
//! the page.
//!
//! ## Why reference-counted instead of borrowed?
//!
//! Returning `&Page` tied to the mmap is unsafe for concurrent access:
//! - A writer calling `grow()` can remap the mmap, invalidating the reference.
//! - A writer freeing a page can reuse it, silently changing the data.
//!
//! `PageRef` owns its page data via an `Arc<Page>`, so it is safe to hold across
//! mmap remaps and page reuse. On a cold read the page is copied out of the mmap
//! once (and CRC-verified); thereafter every cache hit shares that same
//! `Arc<Page>` — the buffer pool and all readers hold clones of one allocation.
//!
//! ## Mutation
//!
//! [`PageRef::into_page`] yields an owned `Page` for write-back. When this is the
//! sole owner of the `Arc` (the common write-path case) it moves out with no
//! copy; when the page is still shared (e.g. resident in the buffer pool) it
//! clones the 16 KB once so the caller's mutation never touches the cached copy.

use std::ops::Deref;
use std::sync::Arc;

use crate::page::Page;

/// Reference-counted page data returned by [`StorageEngine::read_page`].
///
/// Cloning is O(1) (`Arc::clone`). Derefs to `&Page` for transparent use in all
/// existing code paths.
pub struct PageRef {
    inner: Arc<Page>,
}

impl PageRef {
    /// Wraps a heap-allocated `Page` into a `PageRef`.
    ///
    /// `Arc::from(Box<Page>)` reuses the existing heap allocation — no copy.
    pub fn new(page: Box<Page>) -> Self {
        Self {
            inner: Arc::from(page),
        }
    }

    /// Wraps an already shared `Arc<Page>` (e.g. a buffer-pool hit) — no copy.
    pub fn from_arc(page: Arc<Page>) -> Self {
        Self { inner: page }
    }

    /// Consumes the `PageRef` and returns the owned `Page` for mutation.
    ///
    /// Moves out of the `Arc` when this is the sole owner (no copy); falls back
    /// to a single 16 KB clone when the page is still shared (e.g. cached in the
    /// buffer pool), so mutating the result never affects the cached copy.
    pub fn into_page(self) -> Page {
        Arc::try_unwrap(self.inner).unwrap_or_else(|arc| (*arc).clone())
    }

    /// Creates a `PageRef` by copying raw page bytes.
    ///
    /// # Safety
    /// `bytes` must be exactly `PAGE_SIZE` bytes and represent a valid `Page`.
    pub fn from_bytes(bytes: [u8; crate::page::PAGE_SIZE]) -> Self {
        // SAFETY: Page is repr(C, align(64)) and PAGE_SIZE bytes.
        // The array is stack-allocated, then moved to heap via Box.
        let page = unsafe {
            let mut boxed = Box::<Page>::new_uninit();
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                boxed.as_mut_ptr() as *mut u8,
                crate::page::PAGE_SIZE,
            );
            boxed.assume_init()
        };
        Self {
            inner: Arc::from(page),
        }
    }

    /// Test-only: number of `Arc` owners of the underlying page.
    #[cfg(test)]
    pub fn strong_count_for_test(p: &Self) -> usize {
        Arc::strong_count(&p.inner)
    }
}

impl Clone for PageRef {
    /// O(1): bumps the `Arc` refcount, shares the same 16 KB page.
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Deref for PageRef {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.inner
    }
}

impl std::fmt::Debug for PageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageRef")
            .field("page_id", &self.inner.header().page_id)
            .field("page_type", &self.inner.header().page_type)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PAGE_SIZE;

    #[test]
    fn clone_is_shared_not_copied() {
        let pr = PageRef::from_bytes([0u8; PAGE_SIZE]);
        let pr2 = pr.clone();
        // Both observe the same page; clone did not deep-copy into a new alloc.
        assert_eq!(pr.header().page_id, pr2.header().page_id);
        assert_eq!(PageRef::strong_count_for_test(&pr), 2);
        drop(pr2);
        assert_eq!(PageRef::strong_count_for_test(&pr), 1);
    }

    #[test]
    fn into_page_moves_when_sole_owner() {
        let pr = PageRef::from_bytes([0u8; PAGE_SIZE]);
        assert_eq!(PageRef::strong_count_for_test(&pr), 1);
        // Sole owner → moves out of the Arc, no panic.
        let _page: Page = pr.into_page();
    }

    #[test]
    fn into_page_copies_when_shared() {
        let mut bytes = [0u8; PAGE_SIZE];
        bytes[crate::page::HEADER_SIZE] = 0x7E; // a body marker
        let pr = PageRef::from_bytes(bytes);
        // pr2 shares the Arc (strong_count == 2), so into_page on pr must clone
        // while the other handle stays valid.
        let pr2 = pr.clone();
        let p1 = pr.into_page();
        assert_eq!(p1.as_bytes()[crate::page::HEADER_SIZE], 0x7E);
        // pr2 is now the sole owner, so into_page moves with no copy.
        assert_eq!(PageRef::strong_count_for_test(&pr2), 1);
        let p2 = pr2.into_page();
        assert_eq!(p2.as_bytes()[crate::page::HEADER_SIZE], 0x7E);
    }

    #[test]
    fn from_arc_shares_without_copy() {
        let arc = Arc::new(PageRef::from_bytes([0u8; PAGE_SIZE]).into_page());
        let pr = PageRef::from_arc(Arc::clone(&arc));
        assert_eq!(Arc::strong_count(&arc), 2);
        assert_eq!(pr.header().page_id, 0);
    }
}
