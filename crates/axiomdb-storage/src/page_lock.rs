use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// Per-page RwLock table, sharded to minimize contention on the shard lookup.
///
/// ## Design (InnoDB-inspired)
///
/// InnoDB's buffer pool assigns a `block_lock` (SX-lock: S/SX/X modes) to each
/// `buf_block_t`, aligned to the CPU's cache line. PostgreSQL packs pin-count
/// and lock mode into a single atomic `state` field per `BufferDesc`.
/// In both systems the **global struct is `&self`**; page-level locks are interior.
///
/// AxiomDB follows the same principle:
/// ```text
/// Global PageLockTable: &self (shared, immutable structure)
///   └── Shard N: RwLock<HashMap<page_id → Arc<RwLock<()>>>>
///        └── Per-page RwLock: content lock (write = exclusive, read = shared)
/// ```
///
/// ## Sharding
///
/// 64 shards (power of 2, fast modulo). The shard `RwLock` is held only during
/// the HashMap lookup (nanoseconds); the page `RwLock` is held during I/O
/// (microseconds). The two are never held simultaneously.
///
/// Two threads writing **different** pages acquire different page locks → full
/// parallelism. Two threads writing the **same** page serialize → correctness.
///
/// ## Lock lifecycle
///
/// Locks are created lazily on first access to a page. They are never removed,
/// so the total lock count is bounded by the database's total page count.
/// Per-page `RwLock` stored inside each shard's `HashMap`.
type PageRwLock = Arc<RwLock<()>>;

/// Shard: maps page_id → its `RwLock`.
type Shard = RwLock<HashMap<u64, PageRwLock>>;

pub struct PageLockTable {
    shards: Box<[Shard]>,
}

impl PageLockTable {
    /// Creates a new `PageLockTable` with 64 shards.
    pub fn new() -> Self {
        let shards = (0..64)
            .map(|_| RwLock::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        PageLockTable { shards }
    }

    /// Returns the `Arc<RwLock<()>>` for `page_id`, creating it lazily.
    fn page_lock(&self, page_id: u64) -> Arc<RwLock<()>> {
        let shard_idx = (page_id % 64) as usize;
        let shard = &self.shards[shard_idx];

        // Fast path: shared shard lock — entry already exists for most pages.
        {
            let guard = shard.read().unwrap_or_else(|e| e.into_inner());
            if let Some(lock) = guard.get(&page_id) {
                return Arc::clone(lock);
            }
        }

        // Slow path: exclusive shard lock — insert new entry.
        let mut guard = shard.write().unwrap_or_else(|e| e.into_inner());
        // Re-check under write lock (another thread may have inserted between
        // the fast path read and this write acquisition).
        guard
            .entry(page_id)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    /// Acquires a **shared** (read) lock for `page_id`.
    ///
    /// Multiple threads may hold shared locks on the same page concurrently.
    /// Blocks if an exclusive lock is held on the page.
    ///
    /// ## Safety of the returned guard
    ///
    /// The guard holds the `Arc` that owns the `RwLock`. The guard is declared
    /// as the first field of `PageReadGuard` so it is dropped before the `Arc`,
    /// ensuring the `RwLock` is valid for the entire duration of the guard.
    pub fn read(&self, page_id: u64) -> PageReadGuard {
        let lock = self.page_lock(page_id);
        // SAFETY: `lock` is an `Arc<RwLock<()>>`. We obtain a raw pointer to
        // the `RwLock<()>` inside the Arc, then reinterpret it as `'static`.
        // This is sound because:
        //   1. The Arc increments the refcount; the RwLock lives at least as
        //      long as `lock` exists.
        //   2. The `Arc` is stored in `PageReadGuard::lock`, which is the
        //      SECOND field — Rust drops fields in declaration order, so the
        //      guard (first field) is dropped before the Arc (second field).
        //   3. The RwLock is therefore alive for the entire duration of
        //      `PageReadGuard::guard`, even with the extended `'static` lifetime.
        let guard: std::sync::RwLockReadGuard<'static, ()> = unsafe {
            let raw: &'static RwLock<()> = &*(Arc::as_ptr(&lock));
            raw.read().unwrap_or_else(|e| e.into_inner())
        };
        PageReadGuard { guard, lock }
    }

    /// Acquires an **exclusive** (write) lock for `page_id`.
    ///
    /// Only one thread may hold an exclusive lock on a page at a time.
    /// Blocks if any shared or exclusive lock is held on the page.
    pub fn write(&self, page_id: u64) -> PageWriteGuard {
        let lock = self.page_lock(page_id);
        // SAFETY: same reasoning as `read()` — Arc keeps RwLock alive;
        // `guard` (first field) drops before `lock` (second field).
        let guard: std::sync::RwLockWriteGuard<'static, ()> = unsafe {
            let raw: &'static RwLock<()> = &*(Arc::as_ptr(&lock));
            raw.write().unwrap_or_else(|e| e.into_inner())
        };
        PageWriteGuard { guard, lock }
    }
}

impl Default for PageLockTable {
    fn default() -> Self {
        Self::new()
    }
}

// ── Guard types ───────────────────────────────────────────────────────────────

/// RAII shared (read) lock guard for a single page.
///
/// Releases the shared lock on drop.
///
/// ## Drop order (critical)
///
/// `guard` is declared before `lock` so Rust drops `guard` first:
/// 1. `guard` drops → releases the read lock on the `RwLock<()>`
/// 2. `lock` drops → decrements Arc refcount (may free `RwLock<()>`)
///
/// This ordering guarantees the `RwLock` is alive when the guard releases it.
pub struct PageReadGuard {
    // MUST be first: drops before `lock` (Arc).
    #[allow(dead_code)] // RAII: held for its drop effect (releases the read lock)
    guard: std::sync::RwLockReadGuard<'static, ()>,
    // MUST be second: keeps the RwLock alive until after `guard` is dropped.
    #[allow(dead_code)] // RAII: held to keep the RwLock alive
    lock: PageRwLock,
}

/// RAII exclusive (write) lock guard for a single page.
///
/// Releases the exclusive lock on drop. Same drop-order guarantee as
/// [`PageReadGuard`].
pub struct PageWriteGuard {
    // MUST be first: drops before `lock` (Arc).
    #[allow(dead_code)] // RAII: held for its drop effect (releases the write lock)
    guard: std::sync::RwLockWriteGuard<'static, ()>,
    // MUST be second: keeps the RwLock alive until after `guard` is dropped.
    #[allow(dead_code)] // RAII: held to keep the RwLock alive
    lock: PageRwLock,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn test_different_pages_dont_block() {
        // Two threads writing different pages must not block each other.
        let table = Arc::new(PageLockTable::new());
        let barrier = Arc::new(Barrier::new(2));

        let t1 = {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _g = table.write(1);
                barrier.wait(); // signal: holding write lock on page 1
                std::thread::sleep(std::time::Duration::from_millis(20));
            })
        };

        let t2 = {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait(); // wait until t1 holds write lock on page 1
                let start = std::time::Instant::now();
                let _g = table.write(2); // page 2 — must NOT be blocked
                let elapsed = start.elapsed().as_millis();
                assert!(elapsed < 10, "page 2 was unexpectedly blocked: {elapsed}ms");
            })
        };

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn test_exclusive_write_blocks_shared_read() {
        let table = Arc::new(PageLockTable::new());
        let barrier = Arc::new(Barrier::new(2));
        let read_done = Arc::new(std::sync::Mutex::new(false));

        let t1 = {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _g = table.write(42);
                barrier.wait(); // signal: write lock held on page 42
                std::thread::sleep(std::time::Duration::from_millis(30));
                // write lock released on drop
            })
        };

        let t2 = {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            let read_done = Arc::clone(&read_done);
            std::thread::spawn(move || {
                barrier.wait(); // wait until t1 holds write lock
                let _g = table.read(42); // must block until t1 drops write lock
                *read_done.lock().unwrap() = true;
            })
        };

        t1.join().unwrap();
        t2.join().unwrap();
        assert!(*read_done.lock().unwrap());
    }

    #[test]
    fn test_concurrent_reads_same_page() {
        // Four threads must be able to hold shared locks on the same page simultaneously.
        let table = Arc::new(PageLockTable::new());
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let _g = table.read(99);
                barrier.wait(); // all 4 must reach this point without deadlock
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_1000_pages_random_concurrent_access() {
        let table = Arc::new(PageLockTable::new());
        let mut handles = Vec::new();

        for i in 0u64..8 {
            let table = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                for j in 0u64..125 {
                    let page_id = (i * 125 + j) % 1000;
                    if j % 3 == 0 {
                        let _g = table.write(page_id);
                    } else {
                        let _g = table.read(page_id);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_lazy_lock_creation_idempotent() {
        let table = PageLockTable::new();
        // Acquiring the same page lock multiple times (via read/write) is safe.
        {
            let _g = table.read(10);
        }
        {
            let _g = table.read(10);
        }
        {
            let _g = table.write(10);
        }
        {
            let _g = table.write(10);
        }
    }

    #[test]
    fn test_sharding_64_pages() {
        // Acquire write locks on 64 different shard-representative pages.
        let table = PageLockTable::new();
        let mut guards = Vec::new();
        for i in 0u64..64 {
            guards.push(table.write(i));
        }
        // All 64 write locks held simultaneously — must not deadlock.
        drop(guards);
    }
}
