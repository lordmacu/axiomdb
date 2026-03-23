# /unsafe-review — Audit unsafe blocks

## Find all unsafe blocks

```bash
# List all unsafe in the project
grep -rn "unsafe" crates/ --include="*.rs" | grep -v "// SAFETY:"

# See full context of each unsafe
grep -rn -A 5 -B 2 "unsafe {" crates/ --include="*.rs"
```

## For each unsafe block, answer these questions

### 1. Is it really necessary?

Try safe alternatives first:

```rust
// Can it be solved with bytemuck?
use bytemuck::{Pod, Zeroable};
#[repr(C)]
#[derive(Pod, Zeroable, Clone, Copy)]
struct Page { ... }
let page: &Page = bytemuck::from_bytes(&bytes);  // safe!

// Can it be solved with rkyv?
use rkyv::{Archive, Deserialize};
let archived = unsafe { rkyv::access_unchecked::<Page>(&bytes) };
// rkyv handles the unsafe internally with guaranteed invariants

// Can it be restructured to avoid the raw pointer?
```

### 2. What invariant guarantees it is safe?

The SAFETY comment must be specific, not generic:

```rust
// ❌ BAD — too vague
// SAFETY: it is safe

// ❌ BAD — does not explain the invariant
// SAFETY: we trust the pointer is valid

// ✅ GOOD — specific and verifiable
// SAFETY: `ptr` is valid because:
//   1. It comes from `mmap.as_ptr()` which always returns valid memory
//   2. `page_id < self.total_pages` verified at line 42
//   3. The alignment of Page (align=64) is compatible with the mmap pointer
//   4. The mmap lives as long as `StorageEngine` exists (guaranteed by Arc<Mmap>)
let page = unsafe { &*(ptr as *const Page) };
```

### 3. Is there a test that verifies the contract?

```rust
#[test]
fn test_safety_invariant_mmap_pointer() {
    // Verify the unsafe is truly safe at edge cases
    let storage = MmapStorage::create_temp();

    // Edge case: last valid page
    let last_page = storage.total_pages() - 1;
    let result = storage.read_page(last_page);
    assert!(result.is_ok());

    // Verify it fails appropriately out of range
    let result = storage.read_page(storage.total_pages());
    assert!(matches!(result, Err(DbError::PageNotFound { .. })));
}
```

### 4. Is it correctly encapsulated?

```rust
// ❌ BAD — unsafe exposed to caller
pub fn get_page_ptr(id: u64) -> *const Page { ... }

// ✅ GOOD — unsafe encapsulated, public interface is safe
pub fn read_page(&self, id: u64) -> Result<&Page, DbError> {
    if id >= self.total_pages {
        return Err(DbError::PageNotFound { page_id: id });
    }
    let ptr = self.mmap.as_ptr().add(id as usize * PAGE_SIZE);
    // SAFETY: [complete invariant here]
    Ok(unsafe { &*(ptr as *const Page) })
}
```

## Checklist per unsafe block

```
[ ] Did I try bytemuck/rkyv/restructuring first?
[ ] Does the SAFETY comment explain the specific invariant?
[ ] Does the comment mention why each condition holds?
[ ] Is there a test that verifies the contract at edge cases?
[ ] Does the public function have a safe signature even if it uses unsafe internally?
[ ] Did I run miri on this code?
```

```bash
# Verify with miri (detects UB in unsafe)
cargo +nightly miri test unsafe_test_name
```
