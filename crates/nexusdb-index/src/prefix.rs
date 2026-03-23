//! Prefix compression for B+ Tree internal nodes.
//!
//! Internal nodes often have keys with common prefixes (e.g., all starting
//! with `"user:"`). Extracting the prefix once allows storing only the unique
//! suffixes, reducing RAM usage and improving cache locality.
//!
//! This compression operates **in memory only** — the on-disk layout does not change.

/// Internal node with prefix compression.
///
/// # Example
/// ```
/// use nexusdb_index::prefix::CompressedNode;
///
/// let keys: Vec<Box<[u8]>> = vec![
///     b"user:00001".to_vec().into_boxed_slice(),
///     b"user:00002".to_vec().into_boxed_slice(),
///     b"user:00003".to_vec().into_boxed_slice(),
/// ];
/// let children = vec![10u64, 20, 30, 40];
/// let node = CompressedNode::from_keys(&keys, children);
/// // common prefix of "user:00001/2/3" = "user:0000"
/// assert_eq!(node.common_prefix, b"user:0000");
/// assert_eq!(node.reconstruct_key(0), b"user:00001");
/// ```
pub struct CompressedNode {
    /// Common prefix of all keys in the node.
    pub common_prefix: Vec<u8>,
    /// Suffixes (the unique part of each key, without the prefix).
    pub suffixes: Vec<Vec<u8>>,
    /// Pointers to child pages (len = suffixes.len() + 1).
    pub children: Vec<u64>,
}

impl CompressedNode {
    /// Builds a `CompressedNode` from keys and children.
    ///
    /// If all keys share a prefix, it is extracted. If there is no common prefix
    /// (or there are 0 keys), `common_prefix` remains empty.
    pub fn from_keys(keys: &[Box<[u8]>], children: Vec<u64>) -> Self {
        debug_assert_eq!(
            children.len(),
            keys.len() + 1,
            "children.len() must be keys.len() + 1"
        );

        let common_prefix = Self::find_common_prefix(keys);
        let plen = common_prefix.len();
        let suffixes = keys.iter().map(|k| k[plen..].to_vec()).collect();

        Self {
            common_prefix,
            suffixes,
            children,
        }
    }

    /// Reconstructs the full key at position `idx`.
    pub fn reconstruct_key(&self, idx: usize) -> Vec<u8> {
        let mut key = self.common_prefix.clone();
        key.extend_from_slice(&self.suffixes[idx]);
        key
    }

    /// Finds the child page_id for a given `search_key`.
    ///
    /// Equivalent to `find_child_idx` but operating on compressed suffixes.
    pub fn find_child(&self, search_key: &[u8]) -> u64 {
        let n = self.suffixes.len();
        let plen = self.common_prefix.len();

        // Compare with the prefix first
        if search_key.len() < plen || &search_key[..plen] != self.common_prefix.as_slice() {
            // If search_key < common_prefix: go to the first child
            // If search_key > all: go to the last child
            if search_key < self.common_prefix.as_slice() {
                return self.children[0];
            }
            return self.children[n];
        }

        let suffix = &search_key[plen..];
        let child_idx = (0..n)
            .find(|&i| self.suffixes[i].as_slice() > suffix)
            .unwrap_or(n);
        self.children[child_idx]
    }

    /// Finds the child index for a given `search_key`.
    ///
    /// Operates on compressed suffixes — compares only the unique part of each key
    /// after extracting the common prefix. Equivalent to `InternalNodePage::find_child_idx`
    /// but more efficient when the common prefix is long.
    ///
    /// Returns index `j` such that `self.children[j]` contains the range for `search_key`.
    pub fn find_child_idx(&self, search_key: &[u8]) -> usize {
        let n = self.suffixes.len();
        let plen = self.common_prefix.len();

        // If search_key does not start with the common prefix, navigate to extremes
        if search_key.len() < plen || &search_key[..plen] != self.common_prefix.as_slice() {
            return if search_key < self.common_prefix.as_slice() {
                0 // before all keys → first child
            } else {
                n // after all keys → last child
            };
        }

        // Compare only the suffix (the unique part)
        let suffix = &search_key[plen..];
        (0..n)
            .find(|&i| self.suffixes[i].as_slice() > suffix)
            .unwrap_or(n)
    }

    /// Length of the common prefix of a list of keys.
    pub fn common_prefix_len(keys: &[Box<[u8]>]) -> usize {
        Self::find_common_prefix(keys).len()
    }

    /// Calculates the byte savings compared to storing uncompressed keys.
    pub fn bytes_saved(&self) -> usize {
        let plen = self.common_prefix.len();
        plen * self.suffixes.len()
    }

    fn find_common_prefix(keys: &[Box<[u8]>]) -> Vec<u8> {
        if keys.is_empty() {
            return Vec::new();
        }

        let first = &keys[0];
        let mut plen = first.len();

        for key in &keys[1..] {
            plen = first
                .iter()
                .zip(key.iter())
                .take_while(|(a, b)| a == b)
                .count();
            if plen == 0 {
                return Vec::new();
            }
        }
        first[..plen].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bkey(s: &[u8]) -> Box<[u8]> {
        s.to_vec().into_boxed_slice()
    }

    #[test]
    fn test_prefix_extraction() {
        let keys = vec![
            bkey(b"user:00001"),
            bkey(b"user:00002"),
            bkey(b"user:00003"),
        ];
        let children = vec![1u64, 2, 3, 4];
        let node = CompressedNode::from_keys(&keys, children);
        // common prefix = "user:0000" (the 3 differ only in the last digit)
        assert_eq!(node.common_prefix, b"user:0000");
        assert_eq!(node.suffixes[0], b"1");
        assert_eq!(node.suffixes[1], b"2");
        assert_eq!(node.suffixes[2], b"3");
    }

    #[test]
    fn test_reconstruct_key() {
        let keys = vec![bkey(b"order:00100"), bkey(b"order:00200")];
        let children = vec![10u64, 20, 30];
        let node = CompressedNode::from_keys(&keys, children);
        assert_eq!(node.reconstruct_key(0), b"order:00100");
        assert_eq!(node.reconstruct_key(1), b"order:00200");
    }

    #[test]
    fn test_no_common_prefix() {
        let keys = vec![bkey(b"abc"), bkey(b"xyz")];
        let children = vec![1u64, 2, 3];
        let node = CompressedNode::from_keys(&keys, children);
        assert!(node.common_prefix.is_empty());
        assert_eq!(node.suffixes[0], b"abc");
        assert_eq!(node.suffixes[1], b"xyz");
    }

    #[test]
    fn test_find_child() {
        let keys = vec![bkey(b"item:0010"), bkey(b"item:0020"), bkey(b"item:0030")];
        let children = vec![100u64, 200, 300, 400];
        let node = CompressedNode::from_keys(&keys, children.clone());

        assert_eq!(node.find_child(b"item:0005"), 100); // before the first
        assert_eq!(node.find_child(b"item:0010"), 200); // equal to first → right
        assert_eq!(node.find_child(b"item:0015"), 200); // between first and second
        assert_eq!(node.find_child(b"item:0020"), 300);
        assert_eq!(node.find_child(b"item:0030"), 400);
        assert_eq!(node.find_child(b"item:0099"), 400); // after the last
    }

    #[test]
    fn test_bytes_saved() {
        let keys = vec![
            bkey(b"prefix_long:aaa"),
            bkey(b"prefix_long:bbb"),
            bkey(b"prefix_long:ccc"),
        ];
        let children = vec![1u64, 2, 3, 4];
        let node = CompressedNode::from_keys(&keys, children);
        // prefix = "prefix_long:" = 12 bytes × 3 keys = 36 bytes saved
        assert_eq!(node.bytes_saved(), 12 * 3);
    }

    #[test]
    fn test_empty_keys() {
        let node = CompressedNode::from_keys(&[], vec![42]);
        assert!(node.common_prefix.is_empty());
        assert!(node.suffixes.is_empty());
    }
}
