//! Owned page reference returned by [`StorageEngine::read_page`].
//!
//! `PageRef` holds a heap-allocated copy of a page's data. It derefs to
//! `&Page` so callers can use it transparently wherever `&Page` was expected.
//!
//! ## Why owned instead of borrowed?
//!
//! Returning `&Page` tied to the mmap is unsafe for concurrent access:
//! - A writer calling `grow()` can remap the mmap, invalidating the reference.
//! - A writer freeing a page can reuse it, silently changing the data.
//!
//! `PageRef` copies the 16KB page data on `read_page`, making it safe to hold
//! across mmap remaps and page reuse. The copy cost (~0.5µs from L2/L3 cache)
//! is comparable to PostgreSQL's buffer pool copy and SQLite's page cache.

use std::ops::Deref;

use crate::page::Page;

/// Owned page data returned by [`StorageEngine::read_page`].
///
/// Derefs to `&Page` for transparent use in all existing code paths.
pub struct PageRef {
    inner: Box<Page>,
}

impl PageRef {
    /// Wraps a heap-allocated `Page` into a `PageRef`.
    pub fn new(page: Box<Page>) -> Self {
        Self { inner: page }
    }

    /// Consumes the `PageRef` and returns the owned `Page` for mutation.
    ///
    /// This avoids a 16KB copy when the caller needs to modify the page
    /// and write it back (e.g., in-place B-Tree insert without CoW).
    pub fn into_page(self) -> Page {
        *self.inner
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
        Self { inner: page }
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
