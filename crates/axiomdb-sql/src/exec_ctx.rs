/// Bundles the three subsystem references needed by every executor function.
///
/// Inspired by DataFusion's `TaskContext` and DuckDB's `ClientContext`:
/// zero-overhead (raw pointers, no `Arc`), single allocation.
///
/// ## Design rationale
///
/// Previously executor functions received `(storage, txn, bloom)` as three
/// separate parameters — 3 borrows per function call, 100+ call sites. Bundling
/// them into a single `&ExecutionContext` reference reduces the signature width
/// and enables Phase 40.11 to add `LockManager` with a single field addition and
/// zero further signature sweeps.
///
/// ## Mutability model
///
/// The struct stores raw `*const` pointers alongside `PhantomData` that marks
/// the lifetime as invariant. When inner executor functions need `&mut` access
/// (to pass to callee functions that have not yet been migrated to `&dyn`
/// signatures), they use the `storage_mut`, `coord_mut`, and `bloom_mut`
/// accessors, which cast `*const T` → `*mut T` at the point of use.
///
/// This pattern is safe under the single-writer constraint (Phase 3): at most
/// one session executes DML at a time, so no two callers alias the same pointer
/// mutably at the same time.
///
/// `StorageEngine` uses full interior mutability (Phase 40.3) — all trait
/// methods take `&self`. The `&mut dyn StorageEngine` required by many callees
/// is a legacy signature artifact; those callees will be migrated to `&dyn` in
/// Phase 40.11.
///
/// `TxnManager` record operations (`record_index_insert`, `record_index_delete`,
/// `active_snapshot`, `snapshot`) take `&self`. Only `begin/commit/rollback`
/// need `&mut self`, and those are called exclusively at entry-point level.
///
/// `BloomRegistry` mutation methods genuinely require `&mut self`, but are
/// called only inside serialised executor functions (single-writer guarantee).
///
/// ## Phase 40.11 extension
///
/// Adding `lock_mgr` requires one new field here — no further signature sweeps.
pub struct ExecutionContext<'a> {
    /// Raw pointer to the storage engine.
    storage_ptr: *const (dyn axiomdb_storage::StorageEngine + 'a),
    /// Raw pointer to the transaction manager.
    coord_ptr: *const axiomdb_wal::TxnManager,
    /// Raw pointer to the Bloom filter registry.
    bloom_ptr: *const crate::bloom::BloomRegistry,
    /// Invariant lifetime marker tied to the actual borrow.
    _marker: std::marker::PhantomData<&'a ()>,
}

// SAFETY: The raw pointers are obtained from references whose lifetimes are
// tied to `'a`. AxiomDB enforces a single-writer constraint (Phase 3):
// no two sessions execute DML concurrently.
unsafe impl<'a> Send for ExecutionContext<'a> {}

impl<'a> ExecutionContext<'a> {
    /// Constructs an execution context from shared subsystem references.
    ///
    /// Use this constructor when only read access is needed (e.g. SELECT paths).
    pub fn new(
        storage: &'a dyn axiomdb_storage::StorageEngine,
        coord: &'a axiomdb_wal::TxnManager,
        bloom: &'a crate::bloom::BloomRegistry,
    ) -> Self {
        let storage_ptr: *const (dyn axiomdb_storage::StorageEngine + 'a) = storage;
        Self {
            storage_ptr,
            coord_ptr: coord as *const axiomdb_wal::TxnManager,
            bloom_ptr: bloom as *const crate::bloom::BloomRegistry,
            _marker: std::marker::PhantomData,
        }
    }

    /// Constructs an execution context from exclusive (`&mut`) subsystem references.
    ///
    /// Use this constructor at entry points that hold `&mut` refs (e.g.
    /// `execute_with_ctx`). The `&mut` guarantees exclusive ownership, which
    /// makes subsequent calls to `storage_mut`, `coord_mut`, and `bloom_mut`
    /// well-defined.
    pub fn from_mut(
        storage: &'a mut dyn axiomdb_storage::StorageEngine,
        coord: &'a mut axiomdb_wal::TxnManager,
        bloom: &'a mut crate::bloom::BloomRegistry,
    ) -> Self {
        let storage_ptr: *const (dyn axiomdb_storage::StorageEngine + 'a) = storage;
        Self {
            storage_ptr,
            coord_ptr: coord as *const axiomdb_wal::TxnManager,
            bloom_ptr: bloom as *const crate::bloom::BloomRegistry,
            _marker: std::marker::PhantomData,
        }
    }

    /// Returns a shared reference to the storage engine.
    #[inline]
    pub fn storage(&self) -> &'a dyn axiomdb_storage::StorageEngine {
        // SAFETY: pointer was obtained from a valid reference and lives for 'a.
        unsafe { &*self.storage_ptr }
    }

    /// Returns a shared reference to the transaction manager.
    #[inline]
    pub fn coord(&self) -> &'a axiomdb_wal::TxnManager {
        // SAFETY: pointer was obtained from a valid reference and lives for 'a.
        unsafe { &*self.coord_ptr }
    }

    /// Returns a shared reference to the Bloom filter registry.
    #[inline]
    pub fn bloom(&self) -> &'a crate::bloom::BloomRegistry {
        // SAFETY: pointer was obtained from a valid reference and lives for 'a.
        unsafe { &*self.bloom_ptr }
    }

    /// Returns a mutable reference to the storage engine.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other live reference to the same storage
    /// engine exists when this reference is used. Under the single-writer
    /// constraint (Phase 3) this is guaranteed for all AxiomDB executor
    /// functions: at most one statement executes at a time.
    ///
    /// This accessor bridges the gap between `ExecutionContext`'s shared-ref
    /// model and callee functions that still declare `&mut dyn StorageEngine`.
    /// Those callees will be migrated to `&dyn StorageEngine` in Phase 40.11.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn storage_mut(&self) -> &'a mut dyn axiomdb_storage::StorageEngine {
        // SAFETY: see doc comment above. The pointer is valid for 'a and we
        // ensure no aliased mutable references exist (single-writer constraint).
        #[allow(invalid_reference_casting)]
        let mptr: *mut (dyn axiomdb_storage::StorageEngine + 'a) =
            self.storage_ptr as *mut (dyn axiomdb_storage::StorageEngine + 'a);
        &mut *mptr
    }

    /// Returns a mutable reference to the transaction manager.
    ///
    /// # Safety
    ///
    /// Same guarantee as `storage_mut`. The `&mut TxnManager` is required by
    /// callee signatures even though the methods called on it take `&self`.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn coord_mut(&self) -> &'a mut axiomdb_wal::TxnManager {
        // SAFETY: see doc comment above.
        #[allow(invalid_reference_casting)]
        &mut *(self.coord_ptr as *mut axiomdb_wal::TxnManager)
    }

    /// Returns a mutable reference to the Bloom filter registry.
    ///
    /// # Safety
    ///
    /// `BloomRegistry::add` and `mark_dirty` genuinely mutate state. This
    /// accessor is safe under the single-writer constraint (Phase 3): no two
    /// executor call-paths run concurrently, so there is no aliased mutation.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn bloom_mut(&self) -> &'a mut crate::bloom::BloomRegistry {
        // SAFETY: see doc comment above.
        #[allow(invalid_reference_casting)]
        &mut *(self.bloom_ptr as *mut crate::bloom::BloomRegistry)
    }
}
