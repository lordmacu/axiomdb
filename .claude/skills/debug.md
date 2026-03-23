# /debug — Systematic debugging

Never make random changes hoping they work. Follow this process.

## Step 1 — Reproduce with a minimal test

Before investigating, write the smallest possible test that demonstrates the bug:

```rust
#[test]
fn test_bug_reproduction() {
    // Minimal setup — only what is needed to reproduce
    let storage = MemoryStorage::new();

    // Action that causes the bug
    let result = do_thing(storage);

    // Verify the incorrect behavior
    assert_eq!(result, expected); // this must FAIL to confirm the bug
}
```

If you cannot write a test that reproduces the bug, the bug is not well defined.
Go back to the user and ask for more information.

## Step 2 — Formulate hypotheses (minimum 2)

```
Hypothesis A: [what you think is wrong]
  Evidence in favor: [what observations support this]
  How to verify: [concrete experiment]

Hypothesis B: [another possible cause]
  Evidence in favor: [what observations support this]
  How to verify: [concrete experiment]
```

Do not assume the first hypothesis. Always consider at least one alternative.

## Step 3 — Verify each hypothesis

```rust
// Add temporary logging to verify (remove afterwards)
tracing::debug!("value at point X: {:?}", value);

// Or use dbg! for quick values
dbg!(&structure);

// For concurrency: add atomic counters
static COUNTER: AtomicU64 = AtomicU64::new(0);
COUNTER.fetch_add(1, Ordering::Relaxed);
```

Verify hypotheses in order, not in parallel.
Once a hypothesis is discarded, document why.

## Step 4 — Fix in the right place

```
❌ Patch the symptom:
   if result.is_err() { return Ok(default_value); }

✅ Fix the root cause:
   // The problem was that X did not initialize Y correctly
   // Fix: initialize Y before using X
```

The fix must be the minimum change that resolves the root cause.
Do not take the opportunity to "clean up" unrelated code (that goes in a separate commit).

## Step 5 — Regression test

```rust
// The reproduction test from step 1 must now PASS
// Rename it to document the bug it prevents:

#[test]
fn test_btree_does_not_lose_keys_after_split() {
    // This test prevents regression of the bug where the internal node
    // split lost the first key of the right child
    ...
}
```

Add the test to the permanent suite. Never delete it.

## Debugging tools in Rust

```bash
# Show full backtrace
RUST_BACKTRACE=full cargo test test_name

# Sanitizers (detect UB, memory leaks)
RUSTFLAGS="-Z sanitizer=address" cargo +nightly test
RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test  # race conditions

# Miri (detect UB in unsafe code)
cargo +nightly miri test test_name

# For async: tokio-console
cargo add tokio-console
# In code: console_subscriber::init();
```
